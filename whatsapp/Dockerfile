# The WhatsApp service: Node with the system's Chromium (Puppeteer's own download has no
# ARM64 build). Internal only: the Callora server is its one client.
FROM node:24-bookworm-slim AS build
WORKDIR /app
ENV PUPPETEER_SKIP_DOWNLOAD=true
COPY package.json package-lock.json tsconfig.json ./
RUN npm ci --no-audit --no-fund
COPY src ./src
RUN npm run build && npm prune --omit=dev

FROM node:24-bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends chromium fonts-noto-core ca-certificates dumb-init \
    && rm -rf /var/lib/apt/lists/*
ENV NODE_ENV=production \
    PUPPETEER_SKIP_DOWNLOAD=true \
    CHROMIUM_PATH=/usr/bin/chromium \
    DATA_DIR=/data \
    PORT=3100
WORKDIR /app
COPY --from=build /app/node_modules ./node_modules
COPY --from=build /app/dist ./dist
COPY package.json ./
RUN mkdir -p /data && chown node:node /data
USER node
VOLUME ["/data"]
EXPOSE 3100
HEALTHCHECK --interval=15s --timeout=3s --start-period=20s --retries=4 CMD ["node", "-e", "fetch('http://127.0.0.1:3100/health').then(r=>process.exit(r.ok?0:1)).catch(()=>process.exit(1))"]
ENTRYPOINT ["dumb-init", "--"]
CMD ["node", "dist/server.js"]
