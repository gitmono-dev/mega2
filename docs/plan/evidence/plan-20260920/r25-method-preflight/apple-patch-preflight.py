from pathlib import Path
import hashlib,json,re,subprocess,tempfile

cases={
 'insert-start':(b'b\nc\n',b'a\nb\nc\n',0,0,1,1,[b'a\n']),
 'insert-middle':(b'a\nc\n',b'a\nb\nc\n',1,0,2,1,[b'b\n']),
 'insert-end':(b'a\nb\n',b'a\nb\nc\n',2,0,3,1,[b'c\n']),
 'delete-start':(b'a\nb\nc\n',b'b\nc\n',1,1,0,0,[b'a\n']),
 'delete-middle':(b'a\nb\nc\n',b'a\nc\n',2,1,1,0,[b'b\n']),
 'delete-end':(b'a\nb\nc\n',b'a\nb\n',3,1,2,0,[b'c\n']),
 'replace-middle':(b'a\nb\nc\n',b'a\nB\nc\n',2,1,2,1,[b'b\n',b'B\n']),
}
root=Path(tempfile.mkdtemp(prefix='mega2-apple-patch-semantics-'))
version=subprocess.run(['patch','--version'],capture_output=True,text=True,check=True).stdout.splitlines()[0]
records=[]
header=re.compile(rb'^@@ -(\d+),(\d+) \+(\d+),(\d+) @@$',re.MULTILINE)
for name,(old,new,os,oc,ns,nc,edit_lines) in cases.items():
    case=root/name; dry=case/'dry'; actual=case/'actual'
    for d in [dry,actual]: (d/'case').mkdir(parents=True); (d/'case'/'file').write_bytes(new)
    old_lines,new_lines=old.splitlines(keepends=True),new.splitlines(keepends=True)
    if oc==0:
        raw=[b'+'+x for x in edit_lines]
    elif nc==0:
        raw=[b'-'+x for x in edit_lines]
    else:
        raw=[b'-'+edit_lines[0],b'+'+edit_lines[1]]
    canonical=(b'--- a/case/file\n+++ b/case/file\n'+f'@@ -{os},{oc} +{ns},{nc} @@\n'.encode()+b''.join(x if x.endswith(b'\n') else x+b'\n' for x in raw))
    m=header.search(canonical); assert m
    app=canonical
    transforms=[]
    if nc==0:
        adj=ns+1
        app=header.sub(f'@@ -{os},{oc} +{adj},{nc} @@'.encode(),canonical,count=1)
        transforms=[{'canonical_new_start':ns,'application_new_start':adj}]
    patchfile=case/'application.patch'; patchfile.write_bytes(app)
    before=hashlib.sha256(new).hexdigest()
    dryp=subprocess.run(['patch','-C','-R','-p1','-F0','-i',str(patchfile)],cwd=dry,capture_output=True)
    after_dry=hashlib.sha256((dry/'case'/'file').read_bytes()).hexdigest()
    assert dryp.returncode==0,(name,'dry',dryp.stdout,dryp.stderr,canonical,app)
    assert after_dry==before,(name,'dry mutated')
    assert not re.search(rb'(?i)offset|fuzz|FAILED|reject',dryp.stdout+dryp.stderr),(name,dryp.stdout,dryp.stderr)
    apply=subprocess.run(['patch','-R','-p1','-F0','-i',str(patchfile)],cwd=actual,capture_output=True)
    got=(actual/'case'/'file').read_bytes()
    assert apply.returncode==0,(name,'apply',apply.stdout,apply.stderr,canonical,app)
    assert got==old,(name,'restore mismatch',got,old,apply.stdout,apply.stderr,canonical,app)
    assert not re.search(rb'(?i)offset|fuzz|FAILED|reject',apply.stdout+apply.stderr),(name,apply.stdout,apply.stderr)
    records.append({'case':name,'canonical_patch_sha256':hashlib.sha256(canonical).hexdigest(),'application_patch_sha256':hashlib.sha256(app).hexdigest(),'canonical_patch':canonical.decode(),'application_patch':app.decode(),'transforms':transforms,'dry_exit':dryp.returncode,'dry_stdout':dryp.stdout.decode(),'dry_stderr':dryp.stderr.decode(),'dry_sha_before_after':[before,after_dry],'apply_exit':apply.returncode,'apply_stdout':apply.stdout.decode(),'apply_stderr':apply.stderr.decode(),'restored_sha256':hashlib.sha256(got).hexdigest(),'expected_sha256':hashlib.sha256(old).hexdigest()})
summary={'schema':'apple-patch-zero-count-reverse-preflight/v1','patch_version':version,'root':str(root),'cases':records,'result':'PASS'}
Path('/tmp/mega2_apple_patch_semantics_preflight.json').write_text(json.dumps(summary,indent=2)+'\n')
print(json.dumps({'result':'PASS','patch_version':version,'cases':[{'name':r['case'],'transforms':r['transforms'],'restored_sha256':r['restored_sha256']} for r in records],'summary':'/tmp/mega2_apple_patch_semantics_preflight.json','root':str(root)},indent=2))
