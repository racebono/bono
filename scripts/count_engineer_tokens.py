"""Measure the stable engineer system + tool with Anthropic count_tokens.

Usage: python scripts/count_engineer_tokens.py [--ref HEAD] --model claude-sonnet-5-5
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
parser.add_argument('--model', default='claude-sonnet-5-5')
args = parser.parse_args()
root = Path(__file__).resolve().parent.parent


def read(path):
    if args.ref:
        return subprocess.check_output(['git', 'show', f'{args.ref}:{path}'], cwd=root).decode()
    return (root / path).read_text(encoding='utf-8')


skill = read('assets/skill/SKILL.md')
system = skill.split('\n---', 1)[1].lstrip() if skill.startswith('---') else skill
tool = read('src/engineer/tool.rs')
schema = tool.split('pub fn input_schema()', 1)[1].split('serde_json::json!(', 1)[1].split('\n        })', 1)[0] + '\n        }'
schema = json.loads(schema)
description = tool.split('pub fn description()', 1)[1].split('.into()', 1)[0]
description = re.search(r'"(.*)"', description, re.S).group(1)
description = re.sub(r'\\\r?\n\s*', '', description)
tools = [{'name': 'engineer', 'description': description, 'input_schema': schema}]
payload = {'model': args.model, 'system': system, 'tools': tools,
           'messages': [{'role': 'user', 'content': 'Radio check.'}]}
print(json.dumps({'revision': args.ref or 'working tree', 'system_characters': len(system),
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
