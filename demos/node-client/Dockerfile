# Build from the repository root so the contract protos are in context:
#   docker build -f demos/node-client/Dockerfile -t grpc-markup-demo .
#
# lib/markup.js resolves the service contract at ../../../proto relative to
# itself, so with the app at /app the protos belong at /proto. By the same
# relative path the samples resolve to /sample-data.
FROM node:24-alpine
WORKDIR /app
COPY demos/node-client/package.json demos/node-client/package-lock.json ./
RUN npm ci --omit=dev
COPY demos/node-client/server.js ./
COPY demos/node-client/lib ./lib
COPY demos/node-client/public ./public
COPY demos/sample-data /sample-data
COPY proto /proto
ENV PORT=8088
EXPOSE 8088
USER node
# No curl in alpine; busybox wget is. The server answers 200 at UI_BASE.
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s \
  CMD wget -qO /dev/null "http://127.0.0.1:${PORT}${UI_BASE:-/}" || exit 1
CMD ["node", "server.js"]
