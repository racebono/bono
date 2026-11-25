"""Measure the stable engineer system + tool with Anthropic count_tokens.

Usage: python scripts/count_engineer_tokens.py [--ref HEAD] [--sim lmu|iracing|rrre|all] [--no-pit] --model claude-sonnet-5-5
Requires ANTHROPIC_API_KEY for authoritative counts. Never prints the key.
"""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import urllib.request

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--ref', help='Git revision to measure instead of working files')
parser.add_argument('--sim', choices=['lmu', 'iracing', 'rrre', 'all'], default='lmu',
                    help='Simulator prompt scope (default: lmu)')
parser.add_argument('--no-pit', action='store_true',
                    help='Omit skill/pit-service.md (sims without pit-menu control)')
parser.add_argument('--model', default='claude-sonnet-5-5')
args = parser.parse_args()
# RaceRoom has no pit menu control
args.no_pit |= args.sim == 'rrre'
root = Path(__file__).resolve().parent.parent


def read(path):
    if args.ref:
        return subprocess.check_output(['git', 'show', f'{args.ref}:{path}'], cwd=root).decode()
    return (root / path).read_text(encoding='utf-8')


def read_optional(path):
    if args.ref:
        res = subprocess.run(['git', 'show', f'{args.ref}:{path}'], cwd=root, capture_output=True)
        return res.stdout.decode() if res.returncode == 0 else None
    file = root / path
    return file.read_text(encoding='utf-8') if file.exists() else None


def assemble_system():
    skill = read('skill/SKILL.md')
    core = skill.split('\n---', 1)[1].lstrip() if skill.startswith('---') else skill
    sim_paths = (
        ['skill/sim-lmu.md', 'skill/sim-iracing.md', 'skill/sim-rrre.md']
        if args.sim == 'all'
        else [f'skill/sim-{args.sim}.md']
    )
    sim_parts = [read_optional(p) for p in sim_paths]
    pit_part = None if args.no_pit else read_optional('skill/pit-service.md')
    # Older revisions without modular skill files keep SKILL.md intact.
    if all(p is None for p in sim_parts) and read_optional('skill/pit-service.md') is None:
        return core
    parts = [core.rstrip()]
    for part in sim_parts:
        if part and part.strip():
            parts.append(part.strip())
    if pit_part and pit_part.strip():
        parts.append(pit_part.strip())
    return '\n\n'.join(parts)


system = assemble_system()
tool = read('src/engineer/tool.rs')
schema = tool.split('pub fn input_schema()', 1)[1].split('serde_json::json!(', 1)[1].split('\n        })', 1)[0] + '\n        }'
schema = json.loads(schema)
description = tool.split('pub fn description()', 1)[1].split('.into()', 1)[0]
description = re.search(r'"(.*)"', description, re.S).group(1)
description = re.sub(r'\\\r?\n\s*', '', description)
tools = [{'name': 'engineer', 'description': description, 'input_schema': schema}]
payload = {'model': args.model, 'system': system, 'tools': tools,
           'messages': [{'role': 'user', 'content': 'Radio check.'}]}
print(json.dumps({'revision': args.ref or 'working tree', 'sim': args.sim, 'pit': not args.no_pit,
                  'system_characters': len(system),
                  'tool_json_characters': len(json.dumps(tools, ensure_ascii=False)),
                  'model': args.model}))
key = os.environ.get('ANTHROPIC_API_KEY')
if not key:
    print('count_tokens unavailable: ANTHROPIC_API_KEY is not set. No token count claimed.')
    raise SystemExit(0)
request = urllib.request.Request('https://api.anthropic.com/v1/messages/count_tokens',
    data=json.dumps(payload).encode(), headers={'x-api-key': key,
    'anthropic-version': '2023-06-01', 'content-type': 'application/json'})
with urllib.request.urlopen(request, timeout=60) as response:
    print(response.read().decode())
