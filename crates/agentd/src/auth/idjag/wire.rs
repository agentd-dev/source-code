// SPDX-License-Identifier: AGPL-3.0-only
//! Every identifier the ID-JAG flow puts on the wire, in one place: the
//! draft is at -04 (draft-ietf-oauth-identity-assertion-authz-grant) and the
//! MCP extension is young, so a rename in either is a change to this file
//! and nothing else.

/// RFC 8693 `grant_type` of the exchange at the IdP.
pub const GRANT_TOKEN_EXCHANGE: &str = "urn:ietf:params:oauth:grant-type:token-exchange";
/// RFC 7523 `grant_type` of the redemption at the resource's authorization
/// server.
pub const GRANT_JWT_BEARER: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
/// The `requested_token_type` and `issued_token_type` of an ID-JAG (§4.3).
pub const TOKEN_TYPE_ID_JAG: &str = "urn:ietf:params:oauth:token-type:id-jag";
/// RFC 8693 subject token types.
pub const TOKEN_TYPE_ID_TOKEN: &str = "urn:ietf:params:oauth:token-type:id_token";
pub const TOKEN_TYPE_REFRESH_TOKEN: &str = "urn:ietf:params:oauth:token-type:refresh_token";
/// RFC 8693's access token type: the subject and the result of an
/// on-behalf-of exchange at the server's own authorization server (RFC
/// 0044 §5.7, M1b).
pub const TOKEN_TYPE_ACCESS_TOKEN: &str = "urn:ietf:params:oauth:token-type:access_token";
/// The `token_type` an exchange response names for a token that is not an
/// access token (RFC 8693 §2.2.1).
pub const TOKEN_TYPE_NA: &str = "N_A";
/// The JOSE `typ` of an ID-JAG (§3.1).
pub const TYP_ID_JAG: &str = "oauth-id-jag+jwt";
/// An authorization server's metadata member listing the grant profiles it
/// takes, and the ID-JAG profile's identifier (§6.1).
pub const METADATA_GRANT_PROFILES: &str = "authorization_grant_profiles_supported";
pub const PROFILE_ID_JAG: &str = "urn:ietf:params:oauth:grant-profile:id-jag";
/// An IdP's metadata member listing the token types it exchanges for (§6.2).
pub const METADATA_IDENTITY_CHAINING: &str = "identity_chaining_requested_token_types_supported";
/// The MCP Enterprise-Managed Authorization extension, declared on every
/// request to a delegated server.
pub const EMA_EXTENSION: &str = "io.modelcontextprotocol/enterprise-managed-authorization";
/// The step-up error an IdP answers an exchange with (§9.2), and the member
/// that says how recent a sign-in it wants.
pub const INSUFFICIENT_USER_AUTHENTICATION: &str = "insufficient_user_authentication";
pub const MAX_AGE: &str = "max_age";
/// The OAuth error that means a grant or assertion is no good.
pub const INVALID_GRANT: &str = "invalid_grant";
/// The OAuth error a server answers a DPoP proof without its nonce with
/// (RFC 9449 §8).
pub const USE_DPOP_NONCE: &str = "use_dpop_nonce";
