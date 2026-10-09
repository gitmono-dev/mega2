from pathlib import Path
import hashlib,json,re,subprocess,tempfile

header_re=re.compile(rb'^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@$',re.MULTILINE)
noise_re=re.compile(rb'(?i)offset|fuzz|FAILED|reject')
patch_version=subprocess.run(['patch','--version'],capture_output=True,text=True,check=True).stdout.splitlines()[0]
root=Path(tempfile.mkdtemp(prefix='mega2-apple-patch-r26-'))
records=[]

def sha(data): return hashlib.sha256(data).hexdigest()
def patch_bytes(os,oc,ns,nc,old_edits=(),new_edits=(),old_no_newline=False,new_no_newline=False,omit_ones=False):
    def range_text(start,count):
        return str(start) if omit_ones and count==1 else f'{start},{count}'
    header=f'@@ -{range_text(os,oc)} +{range_text(ns,nc)} @@\n'.encode()
    def edit(prefix,items,no_newline):
        out=b''
        for ix,item in enumerate(items):
            raw=prefix+item+b'\n'
            out+=raw
            if ix==len(items)-1 and no_newline: out+=b'\\ No newline at end of file\n'
        return out
    return b'--- a/case/file\n+++ b/case/file\n'+header+edit(b'-',old_edits,old_no_newline)+edit(b'+',new_edits,new_no_newline)
def app_patch(canonical,ns,nc):
    app_start=ns+1 if nc==0 else ns
    if app_start==ns: return canonical,[]
    m=header_re.search(canonical); assert m
    old_count_suffix=','+m.group(2).decode() if m.group(2) else ''
    new_count_suffix=','+str(nc) if m.group(4) else ''
    out,n=header_re.subn(lambda m: f'@@ -{m.group(1).decode()}{old_count_suffix} +{app_start}{new_count_suffix} @@'.encode(),canonical,count=1)
    assert n==1
    return out,[{'canonical_new_start':ns,'application_new_start':app_start}]
def run(argv,cwd):
    p=subprocess.run(argv,cwd=cwd,capture_output=True)
    return {'argv':argv,'exit':p.returncode,'stdout':p.stdout.decode(errors='replace'),'stderr':p.stderr.decode(errors='replace'),'stdout_sha256':sha(p.stdout),'stderr_sha256':sha(p.stderr),'raw_stdout':p.stdout.decode(errors='replace'),'raw_stderr':p.stderr.decode(errors='replace')}
def setup(case,filename,content):
    d=case/filename; d.mkdir(parents=True,exist_ok=True); (d/'file').write_bytes(content); return d

cases=[
 ('insert-start',b'b\nc\n',b'a\nb\nc\n',0,0,1,1,(),(b'a',),False,False),
 ('insert-middle',b'a\nc\n',b'a\nb\nc\n',1,0,2,1,(),(b'b',),False,False),
 ('insert-end',b'a\nb\n',b'a\nb\nc\n',2,0,3,1,(),(b'c',),False,False),
 ('delete-start',b'a\nb\nc\n',b'b\nc\n',1,1,0,0,(b'a',),(),False,False),
 ('delete-middle',b'a\nb\nc\n',b'a\nc\n',2,1,1,0,(b'b',),(),False,False),
 ('delete-end',b'a\nb\nc\n',b'a\nb\n',3,1,2,0,(b'c',),(),False,False),
 ('replace-middle',b'a\nb\nc\n',b'a\nB\nc\n',2,1,2,1,(b'b',),(b'B',),False,False),
 ('delete-multiline-middle',b'a\nb\nc\nd\n',b'a\nd\n',2,2,1,0,(b'b',b'c'),(),False,False),
 ('delete-to-empty',b'a\nb\n',b'',1,2,0,0,(b'a',b'b'),(),False,False),
 ('delete-end-no-newline',b'a\nb\nc',b'a\nb',3,1,2,0,(b'c',),(),True,False),
]
for name,old,new,os,oc,ns,nc,old_edits,new_edits,old_nl,new_nl in cases:
    case=root/name
    forward=patch_bytes(os,oc,ns,nc,old_edits,new_edits,old_nl,new_nl)
    application,transforms=app_patch(forward,ns,nc)
    patchfile=case/'canonical.patch'; appfile=case/'application.patch'
    patchfile.parent.mkdir(parents=True,exist_ok=True); patchfile.write_bytes(forward); appfile.write_bytes(application)
    dry=case/'dry'; inverse=case/'inverse'; forward_copy=case/'forward'
    setup(dry,'case',new); setup(inverse,'case',new); setup(forward_copy,'case',old)
    dry_result=run(['patch','-C','-R','-p1','-F0','-i',str(appfile)],dry)
    dry_sha=sha((dry/'case'/'file').read_bytes())
    assert dry_result['exit']==0,(name,'dry',dry_result,forward,application)
    assert dry_sha==sha(new),(name,'dry mutation')
    assert not noise_re.search((dry_result['stdout']+dry_result['stderr']).encode()),(name,'dry noise',dry_result)
    inverse_result=run(['patch','-R','-p1','-F0','-i',str(appfile)],inverse)
    restored=(inverse/'case'/'file').read_bytes()
    assert inverse_result['exit']==0,(name,'inverse',inverse_result,forward,application)
    assert restored==old,(name,'inverse mismatch',restored,old,inverse_result,forward,application)
    assert not noise_re.search((inverse_result['stdout']+inverse_result['stderr']).encode()),(name,'inverse noise',inverse_result)
    if name=='delete-end-no-newline':
        oldfile=case/'old-source'; newfile=case/'new-source'
        oldfile.write_bytes(old); newfile.write_bytes(new)
        source_diff=subprocess.run(['diff','-U3','--label','a/case/file','--label','b/case/file',str(oldfile),str(newfile)],capture_output=True)
        assert source_diff.returncode==1,(name,'source diff',source_diff.returncode,source_diff.stderr)
        source_patch=case/'source-context.patch'; source_patch.write_bytes(source_diff.stdout)
        forward_result=run(['patch','-p1','-F0','-i',str(source_patch)],forward_copy)
        forward_patch_sha=sha(source_diff.stdout)
    else:
        forward_result=run(['patch','-p1','-F0','-i',str(patchfile)],forward_copy)
        forward_patch_sha=sha(forward)
    forwarded=(forward_copy/'case'/'file').read_bytes()
    assert forward_result['exit']==0,(name,'forward',forward_result,forward)
    assert forwarded==new,(name,'forward mismatch',forwarded,new,forward_result,forward)
    assert not noise_re.search((forward_result['stdout']+forward_result['stderr']).encode()),(name,'forward noise',forward_result)
    records.append({'case':name,'canonical_patch_sha256':sha(forward),'application_patch_sha256':sha(application),'canonical_patch':forward.decode(errors='replace'),'application_patch':application.decode(errors='replace'),'transforms':transforms,'expected_old_sha256':sha(old),'expected_new_sha256':sha(new),'dry_sha_before_after':[sha(new),dry_sha],'dry':dry_result,'inverse':inverse_result,'restored_sha256':sha(restored),'forward':forward_result,'forward_patch_sha256':forward_patch_sha,'forwarded_sha256':sha(forwarded)})

# Omitted-count compatibility for one-line nonzero sides.
name='delete-middle-omitted-counts'; old=b'a\nb\nc\n'; new=b'a\nc\n'
canonical=patch_bytes(2,1,1,0,(b'b',),(),omit_ones=True); application,transforms=app_patch(canonical,1,0)
case=root/name; patchfile=case/'canonical.patch'; appfile=case/'application.patch'; patchfile.parent.mkdir(parents=True); patchfile.write_bytes(canonical); appfile.write_bytes(application)
dry=case/'dry'; inverse=case/'inverse'; forward_copy=case/'forward'; setup(dry,'case',new); setup(inverse,'case',new); setup(forward_copy,'case',old)
checks=[]
for argv,cwd in [(['patch','-C','-R','-p1','-F0','-i',str(appfile)],dry),(['patch','-R','-p1','-F0','-i',str(appfile)],inverse),(['patch','-p1','-F0','-i',str(patchfile)],forward_copy)]: checks.append(run(argv,cwd))
assert [c['exit'] for c in checks]==[0,0,0],checks
assert (dry/'case/file').read_bytes()==new and (inverse/'case/file').read_bytes()==old and (forward_copy/'case/file').read_bytes()==new
records.append({'case':name,'canonical_patch_sha256':sha(canonical),'application_patch_sha256':sha(application),'canonical_patch':canonical.decode(),'application_patch':application.decode(),'transforms':transforms,'expected_old_sha256':sha(old),'expected_new_sha256':sha(new),'dry_sha_before_after':[sha(new),sha((dry/'case/file').read_bytes())],'dry':checks[0],'inverse':checks[1],'restored_sha256':sha((inverse/'case/file').read_bytes()),'forward':checks[2],'forwarded_sha256':sha((forward_copy/'case/file').read_bytes())})

# Adjacent owner-split replacement: reverse insertion first, then reverse deletion; forward uses canonical source order.
name='adjacent-owner-split-replacement'; old=b'a\nold\nc\n'; new=b'a\nnew\nc\n'; case=root/name
canonical_delete=patch_bytes(2,1,1,0,(b'old',),())
app_delete,_=app_patch(canonical_delete,1,0)
canonical_insert=patch_bytes(1,0,2,1,(),(b'new',))
app_insert,_=app_patch(canonical_insert,2,1)
files={'canonical-delete.patch':canonical_delete,'application-delete.patch':app_delete,'canonical-insert.patch':canonical_insert,'application-insert.patch':app_insert}
for fn,data in files.items(): p=case/fn; p.parent.mkdir(parents=True,exist_ok=True); p.write_bytes(data)
dry=case/'dry'; inverse=case/'inverse'; forward_copy=case/'forward'; setup(dry,'case',new); setup(inverse,'case',new); setup(forward_copy,'case',old)
# source ordinals: delete=0, insert=1; reverse order is insert then delete at mapped canonical coords 2 then 1.
drychecks=[]
for patchfile in [case/'application-insert.patch',case/'application-delete.patch']:
 drychecks.append(run(['patch','-C','-R','-p1','-F0','-i',str(patchfile)],dry))
assert all(x['exit']==0 and not noise_re.search((x['stdout']+x['stderr']).encode()) for x in drychecks),drychecks
assert (dry/'case/file').read_bytes()==new
inversechecks=[]
for patchfile in [case/'application-insert.patch',case/'application-delete.patch']:
 inversechecks.append(run(['patch','-R','-p1','-F0','-i',str(patchfile)],inverse))
assert (inverse/'case/file').read_bytes()==old,(inversechecks,(inverse/'case/file').read_bytes())
forwardchecks=[]
for patchfile in [case/'canonical-delete.patch',case/'canonical-insert.patch']:
 forwardchecks.append(run(['patch','-p1','-F0','-i',str(patchfile)],forward_copy))
assert (forward_copy/'case/file').read_bytes()==new,(forwardchecks,(forward_copy/'case/file').read_bytes())
records.append({'case':name,'canonical_patch_sha256':{'delete':sha(canonical_delete),'insert':sha(canonical_insert)},'application_patch_sha256':{'delete':sha(app_delete),'insert':sha(app_insert)},'reverse_order':['insert ordinal 1','delete ordinal 0'],'dry_check_sequence':drychecks,'dry_sha_before_after':[sha(new),sha((dry/'case/file').read_bytes())],'inverse_sequence':inversechecks,'restored_sha256':sha((inverse/'case/file').read_bytes()),'forward_canonical_sequence':forwardchecks,'forwarded_sha256':sha((forward_copy/'case/file').read_bytes())})

# Negative control: canonical middle deletion without Apple application-coordinate transform must not be trusted.
name='negative-untransformed-middle-delete'; old=b'a\nb\nc\n'; new=b'a\nc\n'; case=root/name
canonical=patch_bytes(2,1,1,0,(b'b',),()); patchfile=case/'canonical.patch'; patchfile.parent.mkdir(parents=True); patchfile.write_bytes(canonical)
control=case/'target'; setup(control,'case',new)
negative=run(['patch','-R','-p1','-F0','-i',str(patchfile)],control)
observed=(control/'case/file').read_bytes()
assert negative['exit']==0,negative
assert observed!=old,(name,'negative control unexpectedly restored exact bytes')
records.append({'case':name,'canonical_patch_sha256':sha(canonical),'application_patch_sha256':None,'argv':negative['argv'],'exit':negative['exit'],'stdout':negative['stdout'],'stderr':negative['stderr'],'expected_old_sha256':sha(old),'observed_sha256':sha(observed),'exact_restore':False,'purpose':'demonstrates exit 0 can still misplace a zero-context reverse insertion'})

summary={'schema':'apple-patch-coordinate-preflight/v2','patch_version':patch_version,'evidence_root':str(root),'case_count':len(records),'cases':records,'result':'PASS'}
Path('/tmp/mega2_apple_patch_r26_preflight.json').write_text(json.dumps(summary,indent=2)+'\n')
print(json.dumps({'result':'PASS','patch_version':patch_version,'case_count':len(records),'cases':[r['case'] for r in records],'negative_control':records[-1],'summary':'/tmp/mega2_apple_patch_r26_preflight.json','evidence_root':str(root)},indent=2))
