#!/bin/bash
# Herdr 塔台看板 v3 - 注册表 + 收件箱 + 实时 agent 整合
# 用法: ./dashboard.sh [刷新秒数]

INTERVAL=${1:-3}

trap 'tput cnorm; echo; exit 0' INT TERM

tput civis
tput clear

while true; do
  python3 << PYEOF
import json, subprocess, datetime, sys, os, unicodedata

INTERVAL = ${INTERVAL}
HOME_DIR = os.path.expanduser("~/.herdr-coordinator")
REG_PATH = os.path.join(HOME_DIR, "fleets.json")
INBOX_PATH = os.path.join(HOME_DIR, "inbox.jsonl")
CURSOR_PATH = os.path.join(HOME_DIR, "inbox.cursor")

# ── 宽度计算 ──
def char_width(ch):
    o = ord(ch)
    if o < 0x20: return 0
    if o == 0xFE0F: return 0
    if (0x1F000 <= o <= 0x1FFFF or 0x2700 <= o <= 0x27BF or
        0x2600 <= o <= 0x26FF or 0x2B50 <= o <= 0x2BFF or
        0x23F0 <= o <= 0x23FF or 0x2934 <= o <= 0x2935):
        return 2
    return 2 if unicodedata.east_asian_width(ch) in ('W', 'F') else 1

def display_width(s):
    return sum(char_width(c) for c in s)

def truncate(s, maxw):
    if display_width(s) <= maxw:
        return s
    out, w = '', 0
    for c in s:
        cw = char_width(c)
        if w + cw > maxw - 1:
            return out + '…'
        out += c; w += cw
    return out

def pad(s, target):
    return s + ' ' * max(target - display_width(s), 0)

# ── 数据获取 ──
def run_json(args):
    try:
        r = subprocess.run(args, capture_output=True, text=True, timeout=10)
        return json.loads(r.stdout) if r.returncode == 0 else {}
    except Exception:
        return {}

def load_json_file(path, default):
    try:
        with open(path) as f:
            return json.load(f)
    except Exception:
        return default

agents = run_json(["herdr", "agent", "list"]).get('result', {}).get('agents', [])

# 自动同步注册表（herdr 实时状态 → 注册表 status）
subprocess.run([sys.executable, "fleet", "sync"], capture_output=True, timeout=10)

registry = load_json_file(REG_PATH, {})

inbox = []
try:
    with open(INBOX_PATH) as f:
        lines = f.readlines()
    cursor = 0
    try:
        cursor = int(open(CURSOR_PATH).read().strip() or 0)
    except Exception:
        pass
    for i, line in enumerate(lines):
        try:
            e = json.loads(line)
            e['_unread'] = i >= cursor
            inbox.append(e)
        except Exception:
            pass
except FileNotFoundError:
    pass

# ── 辅助 ──
def status_icon(s):
    return {'working': '🔴', 'idle': '🟢', 'done': '✅', 'blocked': '🟡',
            'need_decision': '🙋', 'unknown': '❓'}.get(s, '❓')

def status_text(s):
    return {'working': '干活中', 'idle': '空闲', 'done': '完成', 'blocked': '卡住',
            'need_decision': '等拍板', 'unknown': '未知'}.get(s, s)

def role_of(name, a, entry, project):
    if name == entry.get('commander'): return '机长'
    kind = a.get('agent', '')
    if kind == 'codex' or 'rev' in name: return '副机长'
    prefix = f'{project}-'
    return name[len(prefix):] if name.startswith(prefix) else name

def ago(ts_str):
    try:
        ts = datetime.datetime.strptime(ts_str, '%Y-%m-%d %H:%M:%S')
        sec = int((datetime.datetime.now() - ts).total_seconds())
        if sec < 60: return f'{sec}秒前'
        if sec < 3600: return f'{sec//60}分钟前'
        if sec < 86400: return f'{sec//3600}小时前'
        return f'{sec//86400}天前'
    except Exception:
        return ts_str or '?'

live = {a.get('name', ''): a for a in agents if a.get('name')}
unnamed = [a for a in agents if not a.get('name')]

# 按项目归属 agent：机长名 / 命名前缀 / 工作目录 三种方式认领
claimed = set()
proj_agents = {}
for proj, entry in registry.items():
    mine = set()
    for n, a in live.items():
        if n in claimed:
            continue
        if (n == entry.get('commander')
                or n.startswith(f'{proj}-')
                or (entry.get('cwd') and a.get('cwd') == entry.get('cwd'))):
            mine.add(n)
    proj_agents[proj] = sorted(mine)
    claimed.update(mine)
orphans = [n for n in sorted(live) if n not in claimed]

# ── 输出 ──
BOX_W = 78
def top(): return '╭' + '─' * BOX_W + '╮'
def mid(): return '├' + '─' * BOX_W + '┤'
def bot(): return '╰' + '─' * BOX_W + '╯'
def L(txt): return '│ ' + pad(truncate(txt, BOX_W - 2), BOX_W - 1) + '│'
def L2(left, right):
    right = truncate(right, 24)
    left = truncate(left, BOX_W - display_width(right) - 3)
    gap = BOX_W - display_width(left) - display_width(right) - 1
    return '│ ' + left + ' ' * max(gap, 1) + right + '│'

now = datetime.datetime.now().strftime('%H:%M:%S')
working = sum(1 for a in agents if a.get('agent_status') == 'working')
blocked = sum(1 for a in agents if a.get('agent_status') == 'blocked')
unread_cnt = sum(1 for e in inbox if e['_unread'])
decisions = [(p, e) for p, e in registry.items() if e.get('status') == 'need_decision']

lines_out = [top()]
lines_out.append(L2(f'🗼 塔台看板  {now}',
                    f'agent {len(agents)} · 🔴{working} · 🟡{blocked}'))

# 待拍板置顶
if decisions:
    lines_out.append(mid())
    lines_out.append(L('🙋 等你拍板：'))
    for p, e in decisions:
        lines_out.append(L(f'   {p} — {e.get("note","")}  ({ago(e.get("updated_at",""))})'))

# 项目区
lines_out.append(mid())
if not registry:
    lines_out.append(L('（注册表为空 — 还没有登记任何项目 fleet）'))
for proj, e in registry.items():
    st = e.get('status', 'unknown')
    header = f'📂 {proj}  {status_icon(st)}{status_text(st)}'
    note = e.get('note', '')
    if note and st != 'need_decision':
        header += f' — {note}'
    lines_out.append(L2(header, ago(e.get('updated_at', ''))))
    names = proj_agents.get(proj, [])
    cmd_name = e.get('commander', f'{proj}-cmd')
    if cmd_name not in live:
        lines_out.append(L(f'   ⚠ 机长 {cmd_name} 已离线！'))
    parts = []
    for n in names:
        a = live[n]
        s = a.get('agent_status', 'unknown')
        parts.append(f'{role_of(n, a, e, proj)}{status_icon(s)}')
    if parts:
        lines_out.append(L('   ' + '  '.join(parts)))
    lines_out.append(L(''))
if registry and lines_out[-1] == L(''):
    lines_out.pop()

# 未归属 agent
if orphans or unnamed:
    lines_out.append(mid())
    lines_out.append(L('📡 未登记的 agent：'))
    for n in orphans:
        a = live[n]
        s = a.get('agent_status', 'unknown')
        cwd = a.get('cwd', '')
        lines_out.append(L2(f'   {status_icon(s)} {n}  ({cwd.split("/")[-1]})', status_text(s)))
    for a in unnamed:
        s = a.get('agent_status', 'unknown')
        title = a.get('terminal_title_stripped', a.get('agent', '?'))
        lines_out.append(L2(f'   {status_icon(s)} {title}', status_text(s)))

# 收件箱
lines_out.append(mid())
label = f'📥 最新汇报（未读 {unread_cnt} 条）' if unread_cnt else '📥 最新汇报'
lines_out.append(L(label))
recent = inbox[-5:]
if not recent:
    lines_out.append(L('   （暂无）'))
for e in reversed(recent):
    mark = '●' if e['_unread'] else ' '
    ts = e.get('ts', '')[11:16]
    lines_out.append(L(f' {mark} {ts} {e.get("project","?")} · '
                       f'{status_text(e.get("status",""))} · {e.get("summary","")}'))

lines_out.append(bot())
sys.stdout.write('\033[H' + '\n'.join(lines_out) + '\n\033[J')
sys.stdout.flush()
PYEOF
  sleep "$INTERVAL"
done
