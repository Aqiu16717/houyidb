# syntax=docker/dockerfile:1

# 构建阶段：安装含开发依赖的完整依赖树并编译 TypeScript。
# EN: Build stage: install the full dependency tree (including dev deps) and compile TypeScript.
FROM node:22-alpine AS build
WORKDIR /app
COPY package.json package-lock.json ./
RUN npm ci
COPY tsconfig.json ./
COPY src ./src
RUN npm run build && npm prune --omit=dev

# 运行阶段：仅保留生产依赖和编译产物，通过 stdio 提供 MCP 服务。
# EN: Runtime stage: keep only production dependencies and compiled output; serve MCP over stdio.
FROM node:22-alpine AS runtime
ENV NODE_ENV=production
WORKDIR /app
COPY --from=build /app/package.json /app/package-lock.json ./
COPY --from=build /app/node_modules ./node_modules
COPY --from=build /app/dist ./dist
USER node
ENTRYPOINT ["node", "dist/index.js"]
