"""Renders the README's terminal pictures from real output.

    python3 docs/images/render.py            # needs q on PATH (or Q_BIN=target/release/q), google-chrome, Pillow

A scenario queue is built in a temporary directory with the q CLI, the
commands are run with color on (q top in a pty while changes land), the
ANSI output becomes HTML in a terminal-window frame, and headless Chrome
renders each page at 2x. The PNGs land next to this script."""
import html, json, os, re, subprocess, sys, time
import tempfile
Q = os.environ.get("Q_BIN", "q")  # Q_BIN=target/release/q renders with a tree build
HERE = os.path.dirname(os.path.abspath(__file__))
WORK = tempfile.mkdtemp(prefix="q-render-")
DB = os.path.join(WORK, "queue.db"); OUT = HERE
ORBIT = WORK  # the cwd for captures; any directory outside a git repo keeps discovery quiet
def q(*args, json_out=False, cwd=ORBIT, env=None):
    cmd = [Q, "--db", DB] + (["--json"] if json_out else []) + list(args)
    r = subprocess.run(cmd, capture_output=True, text=True, cwd=cwd, env=env)
    if r.returncode != 0: print("q failed:", cmd, r.stderr.strip()[:200])
    return json.loads(r.stdout) if json_out and r.stdout.strip() else r.stdout
# ---- scenario ----
if os.path.exists(DB): os.remove(DB)
q("feature", "create", "Live viewer 1.0", "--body", "Ship the browser viewer")
tasks = [
 ("Benchmark producer: fake scopes at an adjustable rate",  ["--project", "orbit", "--feature", "Live viewer 1.0"]),
 ("Self pane: stats row jitters as values change width",    ["--project", "orbit", "--feature", "Live viewer 1.0"]),
 ("Unreal Engine integration guide and plugin",             ["--project", "orbit", "--kind", "research"]),
 ("Nightly wheels for macOS arm64",                         ["--project", "orbit"]),
 ("Rewrite the auth flow",                                  ["--project", "api", "--hold"]),
 ("Compare KV-cache quantization approaches",               ["--project", "ml", "--kind", "research"]),
 ("Track order from a shareable text file, per exe name",  ["--project", "orbit", "--feature", "Live viewer 1.0", "--priority", "2"]),
]
for title, extra in tasks: q(*extra, title)
# 1: done with a PR; 2: in progress at 60%; 3: claimed; 4: review; 6: blocked; 5 held; 7 ready
def claim(agent): 
    d = q("claim", "--agent", agent, json_out=True); return d["task"]["id"], d["claim"]["token"]
tid, tok = claim("claude-01"); q("start", str(tid), "--claim-token", tok, "--branch", "agent/task-1-track-order")
q("log", str(tid), "Parsed the order file; wildcards match on exe name, first match wins", "--progress", "40", "--claim-token", tok)
q("log", str(tid), "Viewer sorts named processes first, then the usual order", "--progress", "80", "--claim-token", tok)
q("complete", str(tid), "--claim-token", tok, "--summary", "Order file applied to the rail; PR opened", "--artifact", "pr=https://github.com/pierricgimmig/orbit/pull/79")
tid, tok = claim("claude-02"); q("start", str(tid), "--claim-token", tok, "--branch", "agent/task-2-benchmark")
q("log", str(tid), "bench.rs: budget spent in whole trees, remainder carried", "--progress", "60", "--claim-token", tok)
TOK2, TID2 = tok, tid
tid, tok = claim("codex-01")   # task 3 stays claimed
tid, tok = claim("claude-03"); q("start", str(tid), "--claim-token", tok, "--branch", "agent/task-4-unreal")
q("log", str(tid), "FExternalProfiler is the hook; plugin registers Orbit", "--progress", "90", "--claim-token", tok)
q("complete", str(tid), "--claim-token", tok, "--summary", "Plugin, patch and guide", "--artifact", "pr=https://github.com/pierricgimmig/orbit/pull/83")
tid, tok = claim("codex-02"); q("block", str(tid), "--claim-token", tok)   # task 6 blocked
print(q("ls", "-a"))
# ---- captures ----
def colored(*args):
    r = subprocess.run([Q, "--db", DB, "--color", "always"] + list(args), capture_output=True, text=True, cwd=ORBIT)
    return r.stdout
caps = {}
caps["q ls"] = colored("ls")
caps["q ls -a"] = colored("ls", "-a")
caps["q tree --feature \"Live viewer 1.0\""] = colored("tree", "--feature", "Live viewer 1.0")
# q top, live: a pty, changes landing while it runs
ts = os.path.join(WORK, "top.typescript")
if os.path.exists(ts): os.remove(ts)
p = subprocess.Popen(["script", "-f", "-q", "-c", f"{Q} --db {DB} top -i 1", ts], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, stdin=subprocess.DEVNULL, cwd=ORBIT, env={**os.environ, "COLUMNS": "110", "LINES": "40"})
time.sleep(1.6)
q("--project", "orbit", "Hover boxes wrap at 500 px; use the window width")
time.sleep(1.2)
q("log", str(TID2), "Window wired; slider sends the rate as it moves", "--progress", "75", "--claim-token", TOK2)
time.sleep(1.2)
q("ready", "5")
time.sleep(2.4)
p.terminate(); time.sleep(0.5); p.kill()
raw = open(ts, "rb").read().decode("utf-8", "replace")
frames = raw.split("\x1b[2J")
frame = [f for f in frames if "recent changes" in f][-1]
frame = frame.replace(DB, "~/.local/share/q/queue.db").replace("\r\n", "\n")
caps["q top"] = frame
caps["q log"] = colored("log", str(TID2))
LOGCMD = f"q log {TID2}"
# ---- ANSI -> HTML ----
COLORS = {30:"#3b3f4a",31:"#f7768e",32:"#9ece6a",33:"#e0af68",34:"#7aa2f7",35:"#bb9af7",36:"#7dcfff",37:"#c0caf5",
          90:"#565f89",91:"#ff9e9e",92:"#a6e3a1",93:"#f2d28a",94:"#9ab8ff",95:"#d3bcff",96:"#a0e0ff",97:"#ffffff"}
def ansi_to_html(text):
    out=[]; st={"b":False,"d":False,"s":False,"fg":None}; link=None
    def span(t):
        if not t: return ""
        css=[]
        if st["fg"]: css.append(f"color:{st['fg']}")
        if st["b"]: css.append("font-weight:700")
        if st["d"]: css.append("opacity:.55")
        if st["s"]: css.append("text-decoration:line-through")
        h=html.escape(t)
        if link: h=f'<a href="{html.escape(link)}" style="color:#7aa2f7;text-decoration:underline">{h}</a>'
        return f'<span style="{";".join(css)}">{h}</span>' if css else h
    pos=0
    for m in re.finditer(r"\x1b\[([0-9;]*)m|\x1b\]8;;([^\x1b\x07]*)(?:\x1b\\|\x07)", text):
        out.append(span(text[pos:m.start()])); pos=m.end()
        if m.group(1) is not None:
            codes=[int(c) for c in m.group(1).split(";") if c] or [0]
            for c in codes:
                if c==0: st.update(b=False,d=False,s=False,fg=None)
                elif c==1: st["b"]=True
                elif c==2: st["d"]=True
                elif c==9: st["s"]=True
                elif c==22: st["b"]=st["d"]=False
                elif c==29: st["s"]=False
                elif c==39: st["fg"]=None
                elif c in COLORS: st["fg"]=COLORS[c]
        else:
            link = m.group(2) or None
    out.append(span(text[pos:]))
    return "".join(out)
PAGE = """<!doctype html><meta charset=utf-8><style>
body{margin:24px;background:#ffffff;width:max-content;font-family:"DejaVu Sans Mono","Liberation Mono",monospace}
.win{display:inline-block;background:#1a1b26;border-radius:10px;box-shadow:0 8px 30px rgba(0,0,0,.45);overflow:hidden;min-width:760px}
.bar{height:34px;background:#24283b;display:flex;align-items:center;padding:0 14px;gap:8px}
.dot{width:12px;height:12px;border-radius:50%%}
.title{margin-left:auto;margin-right:auto;color:#787c99;font-size:12.5px}
pre{margin:0;padding:16px 20px 18px;color:#c0caf5;font-size:13.5px;line-height:1.42;white-space:pre}
.prompt{color:#9ece6a}
</style><div class=win id=win><div class=bar><div class=dot style="background:#ff5f57"></div><div class=dot style="background:#febc2e"></div><div class=dot style="background:#28c840"></div><div class=title>%s</div></div><pre><span class=prompt>$</span> %s
%s</pre></div>"""

def render(name, page):
    import base64
    from PIL import Image, ImageChops
    path = os.path.join(WORK, name + ".html"); open(path, "w").write(page)
    png = os.path.join(WORK, name + ".png")
    subprocess.run(["google-chrome", "--headless=new", "--disable-gpu", "--no-sandbox", "--hide-scrollbars",
                    "--force-device-scale-factor=2", "--window-size=1400,1200", f"--screenshot={png}", "file://" + path],
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True)
    im = Image.open(png).convert("RGB")
    bg = Image.new("RGB", im.size, (255, 255, 255))
    box = ImageChops.difference(im, bg).getbbox()
    m = 8
    box = (max(box[0]-m, 0), max(box[1]-m, 0), min(box[2]+m, im.width), min(box[3]+m, im.height))
    im.crop(box).save(os.path.join(OUT, name + ".png"))
    print(name, "ok", im.crop(box).size)

for name, cmd in (("q-top","q top"),("q-ls","q ls -a"),("q-log","q log"),("q-tree","q tree --feature \"Live viewer 1.0\"")):
    shown = LOGCMD if cmd == "q log" else cmd
    render(name, PAGE % ("q — " + shown, html.escape(shown), ansi_to_html(caps[cmd].rstrip("\n"))))
