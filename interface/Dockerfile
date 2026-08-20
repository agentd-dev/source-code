# SPDX-License-Identifier: AGPL-3.0-only
#
# The hosted web UI (code.agentd.dev) as a static image.
#
# What ships here is a THIN CLIENT and nothing else: one JS bundle, a shell and
# a stylesheet. There is no server-side state, no proxy to anyone's daemon, and
# no credential ever reaches this container — the page talks to the user's own
# agentd directly from their browser, so a compromise of this host leaks static
# assets and nothing more. That property is the reason it can be public at all,
# and it is worth keeping: do not add a backend to this image.
#
#   docker build -f interface/Dockerfile -t agentd-ui interface/
#   docker run --rm -p 8080:8080 agentd-ui

FROM node:22-alpine AS build
WORKDIR /src
# Install with the lockfile so a build is reproducible from the committed tree.
COPY package.json package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY . .
RUN npm run build

# `nginx-unprivileged` listens on 8080 as a non-root user, which is what most
# schedulers want and avoids a CAP_NET_BIND_SERVICE just to serve files.
FROM nginxinc/nginx-unprivileged:1.27-alpine
COPY --from=build /src/dist/web /usr/share/nginx/html
COPY deploy/nginx.conf /etc/nginx/conf.d/default.conf
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s \
  CMD wget -qO- http://127.0.0.1:8080/healthz >/dev/null || exit 1
