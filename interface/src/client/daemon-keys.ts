// SPDX-License-Identifier: AGPL-3.0-only
/**
 * The daemon configuration keys the display clients name in what they tell a
 * person — "list this origin in `a2a.cors.origins`", "turn on
 * `a2a.introspection.enabled`" — kept here so help text never spells a key
 * by hand. A key the daemon renames or removes then breaks one test (every
 * path must resolve in the published config schema) instead of leaving a UI
 * that sends people to edit a setting that no longer exists.
 *
 * Only keys a person sets in the daemon's own config belong here; the
 * clients' own settings (layout, endpoint) are not daemon keys.
 */
export const DAEMON_KEYS = Object.freeze({
  /** Where the A2A listener binds. */
  listen: 'a2a.listen',
  /** The origin the card advertises. */
  url: 'a2a.url',
  /** The operator bearer. */
  bearer: 'a2a.bearer',
  /** The browser origins the listener admits. */
  corsOrigins: 'a2a.cors.origins',
  /** The device grant: how a browser or a remote terminal signs in. */
  deviceGrant: 'a2a.device_grant.enabled',
  /** The observation feed (agentd.events/SubscribeToEvents). */
  events: 'a2a.events.enabled',
  /** The introspection ops the debug views read. */
  introspection: 'a2a.introspection.enabled',
  /** The memory keys `status` publishes, which `memory:<key>` chrome items show. */
  statusValues: 'observability.status_values',
} as const);

export type DaemonKey = (typeof DAEMON_KEYS)[keyof typeof DAEMON_KEYS];
