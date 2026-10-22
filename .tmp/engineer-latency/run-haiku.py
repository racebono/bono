import os, subprocess
from pathlib import Path
root=Path.cwd(); work=root/'.tmp/engineer-latency'
baseline=subprocess.check_output(['git','show','HEAD^:assets/skill/SKILL.md'])
(work/'baseline.md').write_bytes(baseline)
reference=root/'assets/skill/references/engineering.md'
saved=reference.read_bytes()
provider=root/'src/provider/claude.rs'
provider_saved=provider.read_bytes()
env=os.environ.copy()
env.update(EVAL_DB=str(work/'fixtures/telemetry.db'),EVAL_OUT=str(root/'reports/engineer-latency-haiku-high'),EVAL_MODEL='claude-haiku-5-5',EVAL_EFFORT='high',EVAL_IDS='5,8,9,10,29')
try:
    for variant in ['baseline','fixed']:
        reference.write_bytes(saved if variant=='fixed' else subprocess.check_output(['git','show','HEAD^:assets/skill/references/engineering.md']))
        provider.write_bytes(provider_saved if variant=='fixed' else provider_saved.replace(b'            sink.before_tools();\n', b'').replace(b'            sink.before_tools();\r\n', b''))
        env['EVAL_CONFIG']=variant
        env['EVAL_SKILL']=str(root/'assets/skill/SKILL.md' if variant=='fixed' else work/'baseline.md')
        for run in range(1,4):
            env['EVAL_RUN']=f'run-{run}'
            result=subprocess.run(['cargo','test','skill_evals','--no-default-features','--','--ignored','--nocapture'],env=env,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
            (work/f'haiku-high-{variant}-{run}.log').write_text(result.stdout,encoding='utf-8')
            print(variant,run,'exit',result.returncode,flush=True)
            print('\n'.join(line for line in result.stdout.splitlines() if line.startswith('eval ') or 'test result:' in line or 'panicked' in line),flush=True)
            if result.returncode: raise SystemExit(result.returncode)
finally:
    reference.write_bytes(saved)
    provider.write_bytes(provider_saved)

