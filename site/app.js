/* HouyiDB 站点交互，无外部依赖：移动端导航、复制按钮、目录高亮。
   EN: HouyiDB site interactions, no dependencies: mobile navigation, copy
   EN: buttons, and TOC scrollspy. */
(() => {
  "use strict";

  const root = document.documentElement;
  root.classList.remove("no-js");
  root.classList.add("js");

  const isZh = (root.lang || "").toLowerCase().startsWith("zh");

  /* 移动端导航开关。
     EN: Mobile navigation disclosure. */
  const toggle = document.querySelector(".nav-toggle");
  const nav = document.getElementById("site-nav");

  if (toggle && nav) {
    const setOpen = (open) => {
      toggle.setAttribute("aria-expanded", String(open));
      nav.classList.toggle("is-open", open);
      toggle.textContent = open ? (isZh ? "关闭" : "Close") : (isZh ? "菜单" : "Menu");
    };

    toggle.addEventListener("click", () => {
      setOpen(toggle.getAttribute("aria-expanded") !== "true");
    });

    document.addEventListener("keydown", (event) => {
      if (event.key === "Escape" && nav.classList.contains("is-open")) {
        setOpen(false);
        toggle.focus();
      }
    });

    document.addEventListener("click", (event) => {
      if (!nav.classList.contains("is-open")) return;
      if (nav.contains(event.target) || toggle.contains(event.target)) return;
      setOpen(false);
    });

    nav.addEventListener("click", (event) => {
      if (event.target.closest("a")) setOpen(false);
    });

    window.addEventListener("resize", () => {
      if (window.innerWidth > 820) setOpen(false);
    });
  }

  /* 终端代码块复制按钮。
     EN: Copy buttons for terminal blocks. */
  const live = document.createElement("div");
  live.className = "sr-only";
  live.setAttribute("aria-live", "polite");
  document.body.appendChild(live);

  document.querySelectorAll(".terminal").forEach((terminal) => {
    const code = terminal.querySelector("pre code");
    const bar = terminal.querySelector(".terminal-bar");
    if (!code || !bar || bar.querySelector(".copy-btn")) return;

    const button = document.createElement("button");
    button.type = "button";
    button.className = "copy-btn";
    button.textContent = isZh ? "复制" : "Copy";
    button.setAttribute("aria-label", isZh ? "复制代码块" : "Copy code block");
    bar.appendChild(button);

    button.addEventListener("click", async () => {
      const text = code.textContent;
      let copied = false;

      try {
        await navigator.clipboard.writeText(text);
        copied = true;
      } catch (error) {
        const helper = document.createElement("textarea");
        helper.value = text;
        helper.setAttribute("readonly", "");
        helper.style.position = "fixed";
        helper.style.top = "-1000px";
        helper.style.opacity = "0";
        document.body.appendChild(helper);
        helper.select();
        try {
          copied = document.execCommand("copy");
        } catch (fallbackError) {
          copied = false;
        }
        helper.remove();
      }

      if (copied) {
        button.textContent = isZh ? "已复制" : "Copied";
        button.classList.add("is-copied");
        live.textContent = isZh ? "代码已复制到剪贴板" : "Code copied to clipboard";
        window.setTimeout(() => {
          button.textContent = isZh ? "复制" : "Copy";
          button.classList.remove("is-copied");
        }, 1600);
      } else {
        live.textContent = isZh ? "复制失败，请手动选择代码" : "Copy failed; select the code manually";
      }
    });
  });

  /* 性能页目录高亮（渐进增强）。
     EN: TOC scrollspy on the performance page (progressive enhancement). */
  const tocLinks = Array.from(document.querySelectorAll('.toc a[href^="#"]'));
  if (tocLinks.length && "IntersectionObserver" in window) {
    const pairs = tocLinks
      .map((link) => {
        const id = link.getAttribute("href").slice(1);
        const section = id ? document.getElementById(id) : null;
        return section ? { link, section } : null;
      })
      .filter(Boolean);

    const setActive = (id) => {
      pairs.forEach(({ link, section }) => {
        const active = section.id === id;
        link.classList.toggle("is-active", active);
        if (active) link.setAttribute("aria-current", "true");
        else link.removeAttribute("aria-current");
      });
    };

    const observer = new IntersectionObserver(
      (entries) => {
        const visible = entries
          .filter((entry) => entry.isIntersecting)
          .sort((a, b) => a.boundingClientRect.top - b.boundingClientRect.top);
        if (visible.length) setActive(visible[0].target.id);
      },
      { rootMargin: "-18% 0px -68% 0px", threshold: 0 }
    );

    pairs.forEach(({ section }) => observer.observe(section));

    tocLinks.forEach((link) => {
      link.addEventListener("click", () => {
        setActive(link.getAttribute("href").slice(1));
      });
    });
  }
})();
