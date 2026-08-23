#!/usr/bin/env bash
# Workflow-execution micro/meso benchmark: times the REAL reactor over
# representative workflow shapes, per store kind. Usage:
#   bench/wfperf/run.sh [path-to-agentd] [store: memory|file] [label]
# Prints one line per shape: name, wall ms, steps (from step.done count),
# steps/sec. Compare labels across commits; absolute numbers are
# machine-relative.
set -euo pipefail

BIN="${1:-target/release/agentd}"
STORE="${2:-memory}"
LABEL="${3:-baseline}"
DIR="$(mktemp -d /tmp/wfperf.XXXXXX)"
trap 'rm -rf "$DIR"' EXIT

store_block() {
  if [ "$STORE" = file ]; then
    printf 'store:\n  kind: file\n  file: { path: %s/state-%s }\n  checkpoint: { debounce_ms: 0 }\n' "$DIR" "$1"
  else
    printf 'store:\n  kind: memory\n'
  fi
}

common() { # $1 = shape name
  store_block "$1"
  printf 'agent: { name: wfperf }\nlifecycle: { run_until: idle, idle_grace: 400ms }\nobservability: { log_level: info }\n'
}

gen_chain() { # 200 chained assigns, each reading the previous output
  { common chain
    echo 'workflows:'
    echo '  - name: chain'
    echo '    steps:'
    echo '      s: { kind: once }'
    echo '      s0: { kind: assign, depends_on: [s], value: 0 }'
    for i in $(seq 1 200); do
      echo "      s$i: { kind: assign, depends_on: [s$((i-1))], value: \"{{steps.s$((i-1)).output}}x\" }"
    done
    echo "      f: { kind: finish, depends_on: [s200], status: completed }"
  } > "$DIR/chain.yaml"
}

gen_fanout() { # foreach over 300 items, 1-assign body
  { common fanout
    cat <<'YAML'
workflows:
  - name: fanout
    steps:
      s:    { kind: once }
      each: { kind: foreach, depends_on: [s], over: "CEL: [0,1,2,3,4,5,6,7,8,9]", as: d,
              body: { steps: { inner: { kind: foreach, over: "CEL: [0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29]", batch: { size: 30, parallel: 8 },
                      body: { steps: { a: { kind: assign, value: "{{item}}-{{index}}" } } } } } } }
      f:    { kind: finish, depends_on: [each], status: completed }
YAML
  } > "$DIR/fanout.yaml"
}

gen_interp() { # 60 steps, 5 placeholders each over accumulated outputs
  { common interp
    echo 'workflows:'
    echo '  - name: interp'
    echo '    steps:'
    echo '      s: { kind: once }'
    echo '      s0: { kind: assign, depends_on: [s], value: {a: 1, b: "two", c: [1,2,3]} }'
    for i in $(seq 1 60); do
      p=$((i-1))
      echo "      s$i: { kind: assign, depends_on: [s$p], value: \"[{{run.id}}|{{steps.s0.output.a}}|{{steps.s0.output.b}}|{{steps.s$p.output | x}}|{{env.instance}}]\" }"
    done
    echo "      f: { kind: finish, depends_on: [s60], status: completed }"
  } > "$DIR/interp.yaml"
}

gen_cel() { # 100 CEL-gated steps (when: + CEL values)
  { common cel
    echo 'workflows:'
    echo '  - name: celw'
    echo '    steps:'
    echo '      s: { kind: once }'
    echo '      s0: { kind: assign, depends_on: [s], value: 1 }'
    for i in $(seq 1 100); do
      p=$((i-1))
      echo "      s$i: { kind: assign, depends_on: [s$p], when: \"CEL: steps.s$p.status == 'done' && 1 + 1 == 2\", value: \"CEL: int(steps.s0.output) + $i\" }"
    done
    echo "      f: { kind: finish, depends_on: [s100], status: completed }"
  } > "$DIR/cel.yaml"
}

gen_events() { # 120 emits fan into a stream consumer, one run per event
  { common events
    cat <<'YAML'
streams:
  bus: { retention: { max_events: 1000 } }
workflows:
  - name: producer
    steps:
      s:    { kind: once }
      each: { kind: foreach, over: "CEL: [0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,36,37,38,39]", batch: { size: 40, parallel: 8 }, depends_on: [s],
              body: { steps: { pub: { kind: emit, stream: bus, subject: t.ping, data: { n: "{{item}}" } } } } }
      f:    { kind: finish, depends_on: [each], status: completed }
  - name: consumer
    steps:
      take: { kind: stream, stream: bus, subject: "t.*" }
      a:    { kind: assign, depends_on: [take], value: "{{steps.take.output.data.n}}!" }
      f:    { kind: finish, depends_on: [a], status: completed }
YAML
  } > "$DIR/events.yaml"
}

run_one() { # $1 = shape
  local log="$DIR/$1.log" t0 t1 ms steps
  t0=$(date +%s%N)
  "$BIN" --config "$DIR/$1.yaml" >/dev/null 2>"$log" || { echo "$1 FAILED (exit $?)"; tail -3 "$log"; return 1; }
  t1=$(date +%s%N)
  ms=$(( (t1 - t0) / 1000000 ))
  steps=$(grep -c '"event":"step.done"' "$log" || true)
  # idle_grace (400ms) + final drain ride every run; subtract to compare work.
  printf '%-8s %-7s %-10s wall=%6dms steps=%5d steps/sec=%6d\n' \
    "$1" "$STORE" "$LABEL" "$ms" "$steps" "$(( steps * 1000 / (ms > 400 ? ms - 400 : ms) ))"
}

gen_chain; gen_fanout; gen_interp; gen_cel; gen_events
for s in chain fanout interp cel events; do run_one "$s"; done
