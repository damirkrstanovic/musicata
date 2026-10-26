#!/usr/bin/env python3
"""Managed-player pairing and scoped protocol regression, using an isolated server."""
import http.cookiejar
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request

def main():
    with tempfile.TemporaryDirectory() as directory:
        root=Path(directory)
        with socket.socket() as sock:
            sock.bind(('127.0.0.1',0)); port=sock.getsockname()[1]
        base=f'http://127.0.0.1:{port}'
        admin=urllib.request.build_opener(urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar()))
        def api(path,method='GET',body=None,status=200,token=None,as_admin=True):
            headers={'Content-Type':'application/json'}
            if token: headers['Authorization']='Bearer '+token
            req=urllib.request.Request(base+path,data=json.dumps(body).encode() if body is not None else None,headers=headers,method=method)
            try: response=(admin.open if as_admin else urllib.request.urlopen)(req,timeout=10)
            except urllib.error.HTTPError as error: response=error
            data=response.read()
            assert response.status==status,(method,path,response.status,data.decode()[:300])
            return json.loads(data) if data else None
        with (root/'server.log').open('w') as log:
            server=subprocess.Popen([os.environ['MUSICATA_SERVER_BIN'],'--library','testdata-fixture','--database',str(root/'test.db'),'--addr',f'127.0.0.1:{port}'],stdout=log,stderr=log)
            try:
                for _ in range(100):
                    try: api('/api/health');break
                    except OSError:time.sleep(.1)
                api('/api/managed-players/pairings','POST',{'client_id':'speaker-a'},401,as_admin=False)
                api('/api/auth/setup','POST',{'username':'admin','password':'fixture-password'})
                prefix='/api/managed-players'
                pairing=api(prefix+'/pairings','POST',{'client_id':'speaker-a'})
                api(prefix+'/enroll','POST',{'client_id':'wrong-speaker','pairing_token':pairing['pairing_token']},401,as_admin=False)
                host=api(prefix+'/enroll','POST',{'client_id':'speaker-a','pairing_token':pairing['pairing_token']},as_admin=False)
                api(prefix+'/enroll','POST',{'client_id':'speaker-a','pairing_token':pairing['pairing_token']},401,as_admin=False)
                token=host['token']
                report={'inventory_revision':'inventory1','devices':[{'id':'dac','name':'USB DAC','available':True},{'id':'hdmi','name':'HDMI','available':True}], 'selected_device_id':'dac','applied_revision':0,'attempted_revision':0,'status':'idle','error':None,'selection_supported':True}
                api(prefix+'/report','POST',report,401,as_admin=False)
                api(prefix+'/report','POST',{**report,'devices':[report['devices'][0],report['devices'][0]]},400,token=token,as_admin=False)
                desired=api(prefix+'/report','POST',report,token=token,as_admin=False)
                assert desired['revision']==0
                selected=api(prefix+'/speaker-a/device','PUT',{'device_id':'hdmi','inventory_revision':'inventory1'})
                assert selected['desired']['revision']==1
                # The host must learn a newer desired revision while reporting its previous result.
                report['status']='applied'
                desired=api(prefix+'/report','POST',report,token=token,as_admin=False)
                assert desired['revision']==1 and desired['device_id']=='hdmi'
                report.update(applied_revision=1,attempted_revision=1,selected_device_id='hdmi')
                api(prefix+'/report','POST',report,token=token,as_admin=False)
                view=api(prefix+'/speaker-a')
                assert view['paired'] and not view['stale'] and view['report']['applied_revision']==1
                api(prefix+'/speaker-a/device','PUT',{'device_id':'missing','inventory_revision':'inventory1'},400)
                api(prefix+'/speaker-a/device','PUT',{'device_id':'dac','inventory_revision':'outdated'},409)
                api('/api/players',status=401,token=token,as_admin=False)
                api(prefix+'/speaker-a','DELETE',status=204)
                api(prefix+'/report','POST',report,401,token=token,as_admin=False)
                print('PASS one-use pairing, identity binding, scoped credentials, desired/apply cycle, stale inventory and revocation')
            finally:
                server.terminate()
                try:server.wait(timeout=10)
                except subprocess.TimeoutExpired:server.kill();server.wait()

if __name__=='__main__': main()
