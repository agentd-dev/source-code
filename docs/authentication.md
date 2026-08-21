# Authenticating to endpoints

agentd talks to three kinds of outbound endpoint that may require credentials:
**intelligence** (the LLM), **MCP** servers, and **A2A** peers. Each takes an
optional `auth:` block — a unified credential provider (RFC 0031) — so you can
use a static secret, an **interactive OAuth login**, **AWS SigV4**, or a
**SPIFFE** workload identity, with one consistent shape.

> **The rule that ties it together — fail-closed.** A daemon never blocks waiting
> for a human. If an interactive credential is missing and can't be refreshed, the
> process exits telling you to run `agentd --login <target>` rather than hanging.
> Secrets are always `{{secret:…}}` references — never inline, never logged.

## Two contexts

| Context | Credential | Interactive? |
|---|---|---|
| **Headless / cloud** | a static secret (env/file), AWS creds, or a SPIFFE SVID | never |
| **Human at a terminal** | an OAuth login completed once, then a cached, refreshing token | yes, once |

## The `auth:` block

Every endpoint accepts `auth: { kind: …, … }`. The shortcuts
(`intelligence.token`, `mcp.servers[].headers`, `a2a.peers[].client_cert`) are
equivalent to `kind: static`.

```yaml
intelligence:
  endpoints: ["https://llm.corp.example/v1"]
  model: gpt-5.1
  auth: { kind: oauth2, grant: device, issuer: "https://sso.corp.example", client_id: agentd, scopes: [llm.invoke] }

mcp:
  servers:
    - name: github
      endpoint: https://mcp.github.com
      auth: { kind: oauth2, grant: device, issuer: "https://github.com/login/oauth", client_id: "Iv1.abc", scopes: [repo] }

a2a:
  peers:
    - name: partner
      endpoint: https://a2a.partner.example
      auth: { kind: static, token: "{{secret:PARTNER_TOKEN}}" }
```

`agentd --validate-config` checks each block's required fields and rejects an
inline secret.

## Provider kinds

### `static` — a fixed credential

A bearer token or an arbitrary header, from a `{{secret:…}}` reference:

```yaml
auth: { kind: static, token: "{{secret:API_TOKEN}}" }              # → Authorization: Bearer …
auth: { kind: static, header: "X-API-Key", value: "{{secret:KEY}}" }
```

A `{{secret-file:…}}` reference is re-read per request, so a rotated token file is
picked up without a restart.

### `oauth2` — interactive login & machine-to-machine

```yaml
auth:
  kind: oauth2
  issuer: https://sso.corp.example      # or pin token_url / device_authorization_url
  client_id: agentd
  client_secret: "{{secret:OIDC_SECRET}}"   # omit for a public client
  grant: device                         # device | authorization_code | client_credentials
  scopes: [llm.invoke, offline_access]
```

- **`device`** (the default) — the interactive flow (see [`agentd --login`](#agentd-login)).
  Works headless / over SSH; no browser or open port. The token endpoints are
  discovered from `issuer` (RFC 8414 / OIDC) unless pinned.
- **`client_credentials`** — a headless machine-to-machine grant (needs
  `client_secret`); refreshes on its own, no login.
- **`authorization_code`** — a **browser + PKCE** loopback flow. `agentd --login`
  prints the authorization URL (it never shells out to a browser), captures the
  redirect on a one-shot `127.0.0.1` listener, verifies `state`, and exchanges the
  code. Use it when a desktop browser is handier than typing a device code.

### `aws` — SigV4-signed requests

For an endpoint behind AWS IAM (an API-Gateway MCP server, a Bedrock gateway):

```yaml
auth:
  kind: aws
  region: us-east-1
  service: execute-api        # or bedrock, …
  source: env                 # AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY / AWS_SESSION_TOKEN
```

Each request is SigV4-signed (temporary-credential session tokens are handled via
`x-amz-security-token`). Sources: `env`/`static` (the standard `AWS_*` variables),
and **`sso`** — the interactive **IAM Identity Center** login:

```yaml
auth:
  kind: aws
  region: us-east-1
  service: execute-api
  source: sso
  sso_start_url: https://my-org.awsapps.com/start
  account_id: "123456789012"
  role_name: AgentdBedrock
```

`agentd --login mcp:<name>` runs the SSO device flow and caches **temporary AWS
credentials**; the signer reloads them per request (a re-login refreshes them with
no restart).

For **headless** AWS workloads, no login is needed — `source: imds` (an EC2
instance role, IMDSv2) and `source: irsa` (an EKS pod's web identity → STS
`AssumeRoleWithWebIdentity`) fetch and auto-refresh temporary credentials at
runtime:

```yaml
auth: { kind: aws, region: us-east-1, service: bedrock, source: irsa }   # EKS/IRSA
auth: { kind: aws, region: us-east-1, service: bedrock, source: imds }   # EC2 role
```

### `spiffe` — workload identity (SPIFFE/SPIRE)

Zero-secret identity for k8s/cloud, from SPIRE-written files:

```yaml
# JWT-SVID → a rotating bearer (re-read per request):
auth: { kind: spiffe, svid: jwt, jwt_svid_file: /run/spire/jwt.svid }

# X.509-SVID → mutual TLS (needs a TLS build):
auth: { kind: spiffe, svid: x509, svid_file: /run/spire/svid.pem, key_file: /run/spire/key.pem }
```

## Enterprise LLM providers (Azure, Google, AWS)

The enterprise LLMs authenticate with **standard OAuth** (Azure Entra ID, Google) or
**AWS SigV4** — all covered by the providers above. Point `intelligence.endpoints` at
the provider's **OpenAI-compatible** URL and attach the matching `auth:` block.

**Azure OpenAI** — Entra ID (Azure AD) bearer via the device flow:

```yaml
intelligence:
  endpoints: ["https://my-resource.openai.azure.com/openai/deployments/gpt-4o/chat/completions?api-version=2024-08-01-preview"]
  model: gpt-4o
  auth:
    kind: oauth2
    grant: device
    issuer: "https://login.microsoftonline.com/<tenant-id>/v2.0"
    client_id: "<app-registration-client-id>"
    scopes: ["https://cognitiveservices.azure.com/.default"]
```

**Google Vertex AI** — Google OAuth bearer via the device flow, against Vertex's
OpenAI-compatible endpoint:

```yaml
intelligence:
  endpoints: ["https://<region>-aiplatform.googleapis.com/v1/projects/<project>/locations/<region>/endpoints/openapi"]
  model: google/gemini-1.5-pro
  auth:
    kind: oauth2
    grant: device
    issuer: "https://accounts.google.com"
    client_id: "<oauth-client-id>"
    scopes: ["https://www.googleapis.com/auth/cloud-platform"]
```

Complete either once with `agentd --login intelligence`; the daemon refreshes the
token on its own thereafter.

**AWS Bedrock (native)** — talk to the Bedrock runtime directly, no gateway. Set
`intelligence.dialect: bedrock` (agentd speaks the **Bedrock Converse** wire —
tool-calling and system prompts included) and attach a `kind: aws` auth block;
agentd SigV4-signs every dial. The model id rides the URL, so `model:` is the
Bedrock model (or an inference-profile id / ARN):

```yaml
intelligence:
  endpoints: ["https://bedrock-runtime.us-east-1.amazonaws.com"]
  dialect: bedrock
  model: anthropic.claude-3-5-sonnet-20241022-v2:0
  auth:
    kind: aws
    region: us-east-1
    service: bedrock
    source: sso           # or env / static / imds (EC2) / irsa (EKS)
    # for source: sso — the IAM Identity Center portal + role:
    sso_start_url: "https://my-org.awsapps.com/start"
    account_id: "123456789012"
    role_name: AgentdBedrock
```

For `source: sso`, run `agentd --login intelligence` once (the IAM Identity Center
device flow → temporary credentials); the daemon refreshes them thereafter. On EC2
(`imds`) or EKS (`irsa`) the credentials are ambient — no login step. The same
`kind: aws` block also fronts an **AWS-IAM-guarded OpenAI-compatible gateway**
(leave `dialect` unset, set `service: execute-api`); a bearer-guarded gateway takes
`oauth2`/`static` instead.

## <a id="agentd-login"></a>`agentd --login` — the interactive flow

When a provider needs a human (an `oauth2` `device`/`authorization_code` grant),
complete it once with `agentd --login`; the token is cached and the daemon uses
it. The flag needs a binary built with `--features oauth` (the release binaries
and the published image have it).

```console
$ agentd --login mcp:github --config app.yaml
┌─ authorize agentd ───────────────────────────────
│  target   mcp:github
│  visit    https://github.com/login/device
│  code     WDJB-MJHT
│  waiting… (expires in 900s · Ctrl-C to cancel)
└──────────────────────────────────────────────────
logged in to mcp:github — token cached in ~/.local/state/agentd/creds
```

- `agentd --login <target>` — run the flow for `intelligence` or `mcp:<name>`.
- `agentd --logout <target>` — evict the cached credential (no feature needed).

The token (and its refresh token) is cached in a per-user file (`0600`, under
`$AGENTD_CRED_DIR`, else `$XDG_STATE_HOME/agentd/creds`, else
`~/.local/state/agentd/creds`). The daemon reads it at startup and **refreshes**
in memory before expiry — so a long-running agent keeps a live credential without
re-prompting. A restart re-reads the cache; only an expired token with no refresh
token needs a fresh `agentd --login`.

> **Never printed.** The device prompt shows the URL and short code, never the
> token. The cache file holds live tokens and is excluded from all logs, audit,
> and the read surface.

## Where each provider applies

| | static | oauth2 device | oauth2 client-creds | aws (SigV4) | spiffe jwt | spiffe x509 |
|---|---|---|---|---|---|---|
| **MCP server** | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ (mTLS) |
| **intelligence** | ✓ | ✓ | ✓ | ✓ (incl. `dialect: bedrock`) | ✓ | — |
| **A2A peer** | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ (`client_cert`/`key`) |

## Security

- **Fail-closed:** a daemon never blocks on a human; a missing interactive
  credential is a clean startup error naming the `agentd --login` to run.
- **Secret-free:** every credential input is a `{{secret:…}}` / `{{secret-file:…}}`
  reference; the token cache and device codes are never logged.
- **No ambient authority:** every credential is configured per endpoint; AWS/
  SPIFFE sources are opt-in.
- **TLS everywhere:** token and discovery endpoints are HTTPS with certificate
  verification; a public webhook/`auth` bind must use `https://`.

## See also

- [RFC 0031 — Endpoint authentication](../rfcs/0031-endpoint-authentication.md) — the design.
- [Configuration](configuration.md) — every key, precedence, `{{secret:…}}`, `--validate-config`.
- [Security](security.md) — secret handling, the Rule-of-Two, no local execution.
- [Intelligence](intelligence.md) — the LLM wire.
