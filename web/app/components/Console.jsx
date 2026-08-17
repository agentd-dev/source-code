"use client";

import { useEffect, useMemo, useRef, useState } from "react";

/**
 * A replayable terminal demo.
 *
 * Screenshots of a terminal are dead; a video is a 4 MB dependency that does
 * not respond to the reader's theme or reduced-motion preference. This types a
 * scripted session in the page instead: the prompt appears keystroke by
 * keystroke, output lines land with the delays the real thing has, and the
 * whole thing is text — selectable, searchable, and ~2 kB.
 *
 * A script is a list of steps:
 *   {t:'in',  text}            a typed command (with a `$` prompt)
 *   {t:'out', text, cls, ms}   an output line (cls: 'out'|'ok'|'warn'|'err'|'dim')
 *   {t:'wait', ms}             a pause — the spinner runs during it
 *   {t:'spin', text, ms}       a working row that spins for ms, then resolves
 *
 * `prefers-reduced-motion` skips straight to the finished transcript, which is
 * also what a reader sees before hydration.
 */
const SPINNER = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const TYPE_MS = 26;

function useReducedMotion() {
  const [reduced, setReduced] = useState(false);
  useEffect(() => {
    const m = window.matchMedia("(prefers-reduced-motion: reduce)");
    setReduced(m.matches);
    const on = () => setReduced(m.matches);
    m.addEventListener("change", on);
    return () => m.removeEventListener("change", on);
  }, []);
  return reduced;
}

/** Play once when the block first scrolls into view. */
function useInView(ref) {
  const [seen, setSeen] = useState(false);
  useEffect(() => {
    const el = ref.current;
    if (!el || seen) return;
    if (typeof IntersectionObserver === "undefined") {
      setSeen(true);
      return;
    }
    const io = new IntersectionObserver(
      (entries) => entries.forEach((e) => e.isIntersecting && setSeen(true)),
      { rootMargin: "-10% 0px" },
    );
    io.observe(el);
    return () => io.disconnect();
  }, [ref, seen]);
  return seen;
}

export default function Console({
  title = "agentd",
  script = [],
  className = "",
  /**
   * A fixed transcript height (any CSS length).
   *
   * Without it the block grows line by line and the page reflows under the
   * reader — in the hero, where the demo plays while they are still reading the
   * first paragraph, that is the whole layout moving. With it the terminal is a
   * window: it reserves its space up front, scrolls its own content, and
   * switching between demos of different lengths does not move anything either.
   */
  height,
}) {
  const box = useRef(null);
  const view = useRef(null);
  const reduced = useReducedMotion();
  const inView = useInView(box);
  const [done, setDone] = useState([]); // settled lines
  const [typing, setTyping] = useState(null); // the line being typed
  const [spin, setSpin] = useState(null); // {text, frame}
  const [finished, setFinished] = useState(false);

  // The fully-played transcript, used for reduced motion and as the pre-play
  // (SSR) state so the block never renders empty.
  const full = useMemo(
    () => script.filter((s) => s.t === "in" || s.t === "out").map((s) => ({ ...s })),
    [script],
  );

  useEffect(() => {
    if (!inView || reduced) return;
    let cancelled = false;
    let timers = [];
    const sleep = (ms) =>
      new Promise((res) => {
        const id = setTimeout(res, ms);
        timers.push(id);
      });

    (async () => {
      setDone([]);
      setFinished(false);
      for (const step of script) {
        if (cancelled) return;
        if (step.t === "in") {
          for (let i = 1; i <= step.text.length; i++) {
            if (cancelled) return;
            setTyping({ ...step, text: step.text.slice(0, i) });
            await sleep(TYPE_MS);
          }
          await sleep(220);
          setTyping(null);
          setDone((d) => [...d, step]);
        } else if (step.t === "out") {
          setDone((d) => [...d, step]);
          await sleep(step.ms ?? 90);
        } else if (step.t === "spin" || step.t === "wait") {
          const until = Date.now() + (step.ms ?? 900);
          let frame = 0;
          while (Date.now() < until) {
            if (cancelled) return;
            setSpin({ text: step.text ?? "working", frame: frame++ });
            await sleep(80);
          }
          setSpin(null);
        }
      }
      if (!cancelled) setFinished(true);
    })();

    return () => {
      cancelled = true;
      timers.forEach(clearTimeout);
    };
  }, [inView, reduced, script]);

  const lines = reduced || !inView ? full : done;

  // Keep the newest line visible in a fixed-height window; without this the
  // transcript plays on past the bottom edge and the reader watches a static
  // screen. `auto` (not `smooth`) because a per-line animation at typing speed
  // reads as jitter.
  useEffect(() => {
    if (!height) return;
    const el = view.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [height, lines.length, typing, spin]);

  return (
    <div className={`term ${className}`} ref={box}>
      <div className="panel-title">
        <span className="dots">
          <i />
          <i />
          <i />
        </span>
        <span className="ml-1">{title}</span>
        {finished && !reduced && (
          <button
            type="button"
            className="ml-auto text-[0.7rem] text-[#6b6b73] hover:text-[#d4d4d8]"
            onClick={() => {
              setDone([]);
              setFinished(false);
              // re-trigger the effect by nudging the in-view state
              setTyping(null);
              setSpin(null);
              const el = box.current;
              if (el) el.dispatchEvent(new Event("replay"));
              setTimeout(() => setDone([]), 0);
            }}
            aria-label="Replay the demo"
          >
            replay
          </button>
        )}
      </div>
      <pre
        aria-live="polite"
        ref={view}
        style={height ? { height, overflowY: "auto" } : undefined}
      >
        {lines.map((l, i) =>
          l.t === "in" ? (
            <div key={i}>
              <span className="prompt">$ </span>
              {l.text}
            </div>
          ) : (
            <div key={i} className={l.cls || "out"}>
              {l.text}
            </div>
          ),
        )}
        {typing && (
          <div>
            <span className="prompt">$ </span>
            {typing.text}
            <span className="cursor" />
          </div>
        )}
        {spin && (
          <div style={{ color: "#4ade80" }}>
            {SPINNER[spin.frame % SPINNER.length]} {spin.text}
          </div>
        )}
      </pre>
    </div>
  );
}
