# houyidb.top 站点部署说明

> 静态站点（纯 HTML/CSS/JS），无构建步骤、无外部字体/图片/CDN。
> 工作流：`.github/workflows/pages.yml`（GitHub Actions Pages）。
> 自定义域名：`site/CNAME` = `houyidb.top`。

## 1. 目录与预览

```
site/
├── index.html            中文首页
├── performance.html      中文性能页（仅 HouyiDB 自测数据）
├── docs.html             中文文档入口
├── en/                   对应英文页
├── styles.css            基础样式（令牌 / 版式 / 首页组件）
├── pages.css             次级页面（状态 / 页脚 / 图表 / 文档 / 响应式）
├── app.js                移动端导航 / 复制按钮 / 目录高亮
├── CNAME                 houyidb.top
└── README.md             本文件
```

本地预览（仓库根目录）：

```bash
python3 -m http.server 8000 --directory site
# 打开 http://127.0.0.1:8000/
```

`file://` 直接打开也可读，但复制按钮与同源策略相关的行为以 HTTP 预览为准。

## 2. GitHub Pages 部署

> 公共仓 `houyidb-community/houyidb` **尚未创建**；workflow 已写好但未启用。
> 创建仓库之前不要开启 Pages，否则部署会失败。

1. 创建公共仓 `houyidb-community/houyidb`，默认分支设为 `main`。
2. 将 `site/` 与 `.github/workflows/pages.yml` 推送到 `main`。
   > 本机全局忽略文件 `~/.gitignore_global` 第 143 行包含 `site/`，
   > 普通 `git add` 会跳过整个目录。提交前用 `git add -f site`，并确认
   > `git ls-files site | wc -l` 非零；或在公共仓层移除该忽略规则。
   > workflow 只能部署已被 Git 跟踪的 `site/`。
3. 仓库 **Settings → Pages → Build and deployment → Source** 选择
   **GitHub Actions**（不要选 “Deploy from a branch”）。
4. 推送 `main` 或手动运行 workflow **Deploy site to GitHub Pages**；
   它会把 `site/` 作为 artifact 上传并部署，无构建步骤。
5. 首次部署完成后，在 **Settings → Pages** 确认站点 URL；
   `site/CNAME` 已包含 `houyidb.top`，自定义域名应自动填入。
6. DNS 校验通过、证书签发后，勾选 **Enforce HTTPS**。

工作流权限（已写入 `pages.yml`）：

- `contents: read`
- `pages: write`
- `id-token: write`
- job 环境：`github-pages`（部署 URL 由 `deploy-pages` 输出）

## 3. DNS 配置（apex + www）

在域名服务商处为 `houyidb.top` 添加：

| 类型 | 主机记录 | 记录值 |
|------|----------|--------|
| A | `@` | `185.199.108.153` |
| A | `@` | `185.199.109.153` |
| A | `@` | `185.199.110.153` |
| A | `@` | `185.199.111.153` |
| AAAA | `@` | `2606:50c0:8000::153` |
| AAAA | `@` | `2606:50c0:8001::153` |
| AAAA | `@` | `2606:50c0:8002::153` |
| AAAA | `@` | `2606:50c0:8003::153` |
| CNAME | `www` | `houyidb-community.github.io` |

要点：

- **apex 不要用 CNAME**；A/AAAA 与 CNAME 不要在同一主机记录混用。
- 先删除指向旧主机的冲突 A/AAAA/AAAA 记录，TTL 建议 300–3600 秒。
- 若使用 Cloudflare：`@` 与 `www` 均设为 **DNS only（灰云）**；
  GitHub 需要直接校验域名与签发证书。证书就绪后可再评估代理设置。
- 证书签发最长可能需要 24 小时；期间 `https://` 可能不可用，
  可先用 `http://` 或 GitHub 默认域名验证内容。
- 验证：

```bash
dig +short houyidb.top A
dig +short houyidb.top AAAA
dig +short www.houyidb.top CNAME
curl -I https://houyidb.top/
```

- 若仓库改名或迁移组织，需要同步更新 `www` 的 CNAME 目标与
  `site/CNAME`，并在 Pages 设置中重新校验域名。

## 4. 内容约定

- 性能页数字只来自
  `docs/benchmarks/houyidb-performance-2026-10.md`；不加入外部系统数据、
  不写跨场景总分、不写对比性排名结论。
- 中文页与 `site/en/` 英文页保持同构，新增章节请同时补齐两种语言。
- 不引入外部字体、图片、脚本或 CDN；图标一律内联 SVG。
- 文档入口指向 GitHub `main` 分支；MCP 在发布前保持 “coming soon”，
  不编造安装命令。

## 5. 上线前自检

```bash
# HTML 基本闭合检查（示例，按需替换为 tidy）
python3 - <<'PY'
import glob
from html.parser import HTMLParser

VOID = {"area","base","br","col","embed","hr","img","input","link","meta","param","source","track","wbr"}

class Check(HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.stack = []
        self.errors = []
    def handle_starttag(self, tag, attrs):
        if tag not in VOID:
            self.stack.append(tag)
    def handle_startendtag(self, tag, attrs):
        pass
    def handle_endtag(self, tag):
        if tag in VOID:
            return
        if not self.stack or self.stack[-1] != tag:
            self.errors.append(f"unexpected </{tag}> at {self.getpos()}")
            if tag in self.stack:
                while self.stack and self.stack.pop() != tag:
                    pass
        else:
            self.stack.pop()

for path in sorted(glob.glob("site/**/*.html", recursive=True)):
    parser = Check()
    parser.feed(open(path, encoding="utf-8").read())
    if parser.stack or parser.errors:
        print(path, "FAIL", parser.stack[:5], parser.errors[:3])
    else:
        print(path, "OK")
PY
```

同时确认本地相对链接均存在、`CNAME` 内容为单行 `houyidb.top`，
并分别在 320 / 768 / 1440 宽度与键盘导航下检查。
