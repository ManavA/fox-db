import sys,re,os,subprocess,shutil
src,dst=sys.argv[1],sys.argv[2]
deny=re.compile(r'(^|/)(\.agent-review|\.DerivedData[^/]*|\.build|\.claude|\.Archives|\.Exports|\.DeviceBuilds|exports|\.server-work|docs/design|docs/captures)(/|$)|\.(p8|p12|cer|mobileprovision)$|(^|/)AuthKey_|(^|/)\.env|Local\.xcconfig$|\.xcarchive/')
code=re.compile(r'\.(swift|kt|kts|py|sh|m|h|c|mm|js|ts|json|yml|yaml|toml)$')
out=subprocess.run(['git','-C',src,'ls-files','-z'],capture_output=True,check=True).stdout
n=sw=0
for p in out.split(b'\0'):
  p=p.decode()
  if not p or deny.search(p) or not code.search(p): continue
  s=os.path.join(src,p)
  if not os.path.isfile(s) or os.path.islink(s): continue
  d=os.path.join(dst,p); os.makedirs(os.path.dirname(d),exist_ok=True)
  subprocess.run(['cp','-c',s,d],check=True); n+=1; sw+=p.endswith('.swift')
print(n,'files',sw,'swift')
