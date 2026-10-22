import os, subprocess, pathlib, tomllib
root=pathlib.Path.cwd()
work=root/'.tmp/engineer-latency'
config=tomllib.loads((root/'.cargo/config.priv.toml').read_text())
env=os.environ.copy()
for k,v in config.get('env',{}).items():
    if isinstance(v,str): env[k]=v
    elif isinstance(v,dict) and 'value' in v: env[k]=v['value']
for ref in ['HEAD^','HEAD']:
    subprocess.run(['python','scripts/count_engineer_tokens.py','--ref',ref],env=env,check=True)
