# 生成 README 顶部的 60 秒演示 GIF / Generate the README demo GIF.
# 依赖 / Requires: brew install vhs
# 用法 / Usage: vhs demo/demo.tape

Output demo/houyidb-demo.gif

Set Shell "bash"
Set FontSize 14
Set FontFamily "Menlo, Monaco, monospace"
Set Width 1100
Set Height 760
Set Padding 24
Set Framerate 30
Set TypingSpeed 30ms

Set Theme {
  "background": "#0c0b0a",
  "foreground": "#f1eadf",
  "black": "#0c0b0a",
  "red": "#dd2626",
  "green": "#7fbf7f",
  "yellow": "#e0b35a",
  "blue": "#7aa2d6",
  "magenta": "#c58ad6",
  "cyan": "#6fc7c7",
  "white": "#f1eadf",
  "brightBlack": "#8a8173",
  "brightRed": "#ff5c5c",
  "brightGreen": "#a6d6a6",
  "brightYellow": "#f0c97a",
  "brightBlue": "#9ab8e0",
  "brightMagenta": "#d9a6e8",
  "brightCyan": "#8fdcdc",
  "brightWhite": "#ffffff"
}

Type "./demo/demo.sh"
Enter
Sleep 22s
