import json,re,statistics
from pathlib import Path
root=Path('reports/engineer-latency-haiku-high')
rows=[]
for p in root.glob('eval-*/*/run-*/timing.json'):
    t=json.loads(p.read_text()); events=json.loads(p.with_name('events.json').read_text())
    text=''; first=None; ready=None; before=''; tool=None
    for e in events:
        if e['kind']=='tool_start' and tool is None: tool=e['ms']; before=text
        if e['kind']=='text':
            if first is None and e['text'].strip(): first=e['ms']
            text+=e['text']
            if ready is None and (re.search(r'[.!?]\s',text) or '\n' in text): ready=e['ms']
    if ready is None: ready=t['duration_ms']
    rows.append(dict(variant=p.parts[-3],id=int(p.parts[-4].split('-')[1]),run=p.parts[-2],first_ms=first,ready_ms=ready,tool_ms=tool,early_ready=tool is not None and ready<=tool,pre_tool_text=before,**t))
for variant in ['baseline','candidate','fixed']:
    for group,ids in [('short',[5,8]),('long',[9,10,29])]:
        rs=[r for r in rows if r['variant']==variant and r['id'] in ids]
        if rs:
            print(variant,group,len(rs),json.dumps({k:round(statistics.median(r[k] for r in rs),1) for k in ['first_ms','ready_ms','duration_ms','input_tokens','output_tokens','total_tokens']}),'early',sum(r['early_ready'] for r in rs),'tools',sum(r['tool_calls'] for r in rs))
(root/'measurements.json').write_text(json.dumps(rows,indent=2))

