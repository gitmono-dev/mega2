from pathlib import Path
import hashlib,json,subprocess,tempfile
cases={
 'delete-start':(b'a\nb\nc\n',b'b\nc\n',b'@@ -1,1 +0,0 @@'),
 'delete-middle':(b'a\nb\nc\n',b'a\nc\n',b'@@ -2,1 +1,0 @@'),
 'delete-end':(b'a\nb\nc\n',b'a\nb\n',b'@@ -3,1 +2,0 @@'),
 'insert-start':(b'b\nc\n',b'a\nb\nc\n',b'@@ -0,0 +1,1 @@'),
}
root=Path(tempfile.mkdtemp(prefix='libra-zero-count-r26-'))
records=[]
for name,(old,new,expected) in cases.items():
 repo=root/name
 init=subprocess.run(['libra','init','--vault','false','--quiet',str(repo)],capture_output=True)
 assert init.returncode==0,(name,'init',init.stdout,init.stderr)
 (repo/'f').write_bytes(old)
 add=subprocess.run(['libra','add','f'],cwd=repo,capture_output=True)
 assert add.returncode==0,(name,'add',add.stdout,add.stderr)
 (repo/'f').write_bytes(new)
 argv=['libra','diff','--unified=0','--no-color','--no-pager','--','f']
 p=subprocess.run(argv,cwd=repo,capture_output=True)
 assert p.returncode in (0,1),(name,p.returncode,p.stderr)
 assert expected in p.stdout,(name,expected,p.stdout)
 records.append({'case':name,'argv':argv,'exit':p.returncode,'stdout':p.stdout.decode(),'stderr':p.stderr.decode(),'stdout_sha256':hashlib.sha256(p.stdout).hexdigest(),'stderr_sha256':hashlib.sha256(p.stderr).hexdigest(),'expected_hunk':expected.decode()})
summary={'schema':'libra-zero-count-coordinate-fixture/v1','root':str(root),'cases':records,'result':'PASS'}
Path('/tmp/mega2_libra_zero_count_fixture.json').write_text(json.dumps(summary,indent=2)+'\n')
print(json.dumps({'result':'PASS','cases':[{'case':x['case'],'expected_hunk':x['expected_hunk'],'exit':x['exit']} for x in records],'summary':'/tmp/mega2_libra_zero_count_fixture.json','root':str(root)},indent=2))
