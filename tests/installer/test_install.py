import importlib.util
from contextlib import closing
import sys
import tempfile
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location('musicata_install', Path(__file__).resolve().parents[2] / 'packaging/install.py')
m = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = m
spec.loader.exec_module(m)

class PlanningTests(unittest.TestCase):
    def test_cloudflare_docker_pulls_prebuilt_image_without_building(self):
        from unittest.mock import patch
        cfg=m.options(['--with','caddy','--tls-domain','music.example.org','--tls-dns','cloudflare','--tls-dns-token-file','/root/cloudflare-token'])
        cfg['version']='1.3.0'
        commands=[]
        def docker(argv):
            commands.append(list(map(str,argv)))
            if argv[:3]==['docker','image','inspect']: return 'sha256:fixture'
            return ''
        with tempfile.TemporaryDirectory() as scratch, patch.object(m,'run',side_effect=docker):
            images=m.docker_images(cfg,Path(scratch))
            self.assertFalse(list(Path(scratch).iterdir()))
        self.assertFalse(any(c[:2]==['docker','build'] for c in commands))
        pulls=[c[2] for c in commands if c[:2]==['docker','pull']]
        self.assertTrue(f'ghcr.io/{m.REPO}-caddy:1.3.1' in pulls)
        self.assertEqual(images['caddy'],'sha256:fixture')

    def test_cloudflare_image_pull_failure_precedes_service_changes(self):
        from unittest.mock import patch
        cfg=m.options(['--with','caddy','--tls-domain','music.example.org','--tls-dns','cloudflare','--tls-dns-token-file','/root/cloudflare-token'])
        cfg['version']='1.3.0'
        def docker(argv):
            if argv[:2]==['docker','pull'] and '-caddy:' in argv[2]:
                raise m.InstallError('registry unavailable')
            return 'sha256:fixture'
        with tempfile.TemporaryDirectory() as scratch, patch.object(m,'run',side_effect=docker):
            with self.assertRaisesRegex(m.InstallError,'registry unavailable'):
                m.docker_images(cfg,Path(scratch))

    def test_distribution_families_and_derivatives(self):
        for distro, like, expected in [('ubuntu','','debian'),('debian','','debian'),('cachyos','arch','arch'),('endeavouros','arch','arch'),('linuxmint','ubuntu debian','debian'),('fedora','','fedora')]:
            self.assertEqual(m.distro_family({'ID':distro,'ID_LIKE':like}), expected)
        with self.assertRaises(m.InstallError): m.distro_family({'ID':'alpine'})

    def test_os_release_is_data_not_shell_code(self):
        self.assertEqual(m.parse_os_release('ID=cachyos\nID_LIKE="arch"\nNAME="$(touch /tmp/no)"')['ID_LIKE'],'arch')

    def test_default_docker_and_ml_rejected_for_native(self):
        self.assertEqual(m.options([])['mode'],'docker')
        with self.assertRaises(m.InstallError): m.options(['--mode','native','--with','ml'])

    def test_cpu_baseline_and_architecture(self):
        flags=set('lm sse sse2 pni ssse3 sse4_1 sse4_2 popcnt cx16 lahf_lm'.split())
        self.assertEqual(m.cpu_issues('x86_64',flags,'docker',False),[])
        self.assertTrue(m.cpu_issues('x86_64',flags-{'sse4_2'},'native',False))
        self.assertEqual(m.cpu_issues('aarch64',set(),'native',False),[])
        self.assertTrue(m.cpu_issues('aarch64',set(),'docker',False))
        self.assertTrue(m.cpu_issues('x86_64',flags,'docker',True))
        v3=flags|set('avx avx2 bmi1 bmi2 f16c fma abm movbe xsave'.split())
        self.assertEqual(m.cpu_issues('x86_64',v3,'docker',True),[])

    def test_mode_switch_and_unrequested_component_removal_are_rejected(self):
        old=m.options(['--mode','native','--with','mpd','--user','damirk'])
        new=m.options(['--upgrade'],old)
        self.assertEqual(new['mode'],'native')
        self.assertEqual(new['components'],['mpd'])
        self.assertEqual(new['user'],'damirk')
        with self.assertRaises(m.InstallError): m.options(['--upgrade','--mode','docker'],old)

    def test_dsp_preparation_is_explicit_and_preserves_dac_target(self):
        with self.assertRaises(m.InstallError): m.options(['--mode','native','--with','dsp'])
        with self.assertRaises(m.InstallError): m.options(['--mode','docker','--with','mpd,dsp'])
        cfg = m.options(['--mode','native','--with','mpd,dsp','--alsa-device','hw:CARD=USB,DEV=0'])
        processor = m.processor_config(cfg)
        self.assertEqual(processor['devices']['capture']['device'],'hw:Loopback,1,0')
        self.assertEqual(processor['devices']['playback']['device'],'hw:CARD=USB,DEV=0')
        self.assertEqual(processor['devices']['samplerate'],48000)
        self.assertEqual(processor['pipeline'],[])

    def test_package_selection(self):
        for family, engine in [('debian','docker.io'),('arch','docker'),('fedora','moby-engine')]:
            self.assertIn(engine,m.packages(family,m.options([])))
            self.assertIn('mpd',m.packages(family,m.options(['--mode','native','--with','mpd'])))
        self.assertIn('snapcast',m.packages('arch',m.options(['--mode','native','--with','snapcast'])))

    def test_caddy_requires_a_public_hostname_and_safe_values(self):
        for args in [['--with','caddy'], ['--tls-domain','music.example.org'],
                     ['--with','caddy','--tls-domain','musicata.local'],
                     ['--with','caddy','--tls-domain','192.168.2.144'],
                     ['--with','caddy','--tls-domain','music.example.org\nadmin off'],
                     ['--with','caddy','--tls-domain','music.example.org','--port','443'],
                     ['--with','caddy','--tls-domain','music.example.org','--tls-email','bad\nemail']]:
            with self.subTest(args=args), self.assertRaises((m.InstallError,SystemExit)):
                m.options(args)

    def test_caddy_plans_native_package_and_docker_sidecar(self):
        for mode in ('native','docker'):
            cfg=m.options(['--mode',mode,'--with','caddy','--tls-domain','music.example.org','--tls-email','admin@example.org'])
            self.assertEqual(cfg['tls_domain'],'music.example.org')
            for family in ('debian','arch','fedora'):
                self.assertEqual('caddy' in m.packages(family,cfg),mode=='native')
            self.assertIn('MUSICATA_ADDR=127.0.0.1:3030',m.unit_text(cfg,1001,1001))
            self.assertIn('MUSICATA_ADDR=127.0.0.1:3030',m.docker_command(cfg,1001,1001,'server','sha256:test'))

    def test_upgrade_can_add_caddy_and_then_preserves_tls_settings(self):
        old=m.options(['--mode','native','--with','mpd'])
        upgraded=m.options(['--upgrade','--with','mpd,caddy','--tls-domain','music.example.org'],old)
        self.assertEqual(upgraded['components'],['caddy','mpd'])
        self.assertEqual(m.options(['--upgrade'],upgraded)['tls_domain'],'music.example.org')
        with self.assertRaises(m.InstallError):
            m.options(['--upgrade','--tls-domain','other.example.org'],upgraded)
        with self.assertRaises(m.InstallError):
            m.options(['--upgrade','--with','mpd'],upgraded)

    def test_caddy_configuration_uses_acme_and_secures_only_the_session_cookie(self):
        cfg=m.options(['--with','caddy','--tls-domain','music.example.org','--tls-email','admin@example.org'])
        text=m.caddy_text(cfg)
        self.assertIn('https://acme-v02.api.letsencrypt.org/directory',text)
        self.assertIn('email admin@example.org',text)
        self.assertIn('reverse_proxy 127.0.0.1:3030',text)
        self.assertIn('musicata_session=',text)
        self.assertIn('Secure',text)
        self.assertIn('admin off',text)
        argv=m.docker_command(cfg,1001,1001,'caddy','sha256:caddy')
        self.assertIn('musicata-caddy',argv)
        self.assertIn(str(m.CADDY_ROOT/'data')+':/data:z',argv)
        self.assertIn(str(m.CADDY_ROOT/'config')+':/config:z',argv)
        self.assertEqual(argv[argv.index('sha256:caddy')+1:][:2],['caddy','run'])

    def test_cloudflare_plan_preserves_provider_and_keeps_secrets_out_of_config(self):
        cfg=m.options(['--mode','native','--with','caddy','--tls-domain','music.example.org',
                       '--tls-dns','cloudflare','--tls-dns-token-file','/root/cloudflare-token'])
        self.assertIn('golang-go',m.packages('debian',cfg))
        text=m.caddy_text(cfg)
        self.assertIn('dns cloudflare {env.CLOUDFLARE_API_TOKEN}',text)
        self.assertIn('resolvers 1.1.1.1 1.0.0.1',text)
        self.assertNotIn('/root/cloudflare-token',text)
        self.assertIn('EnvironmentFile=',m.caddy_unit_text(cfg,1001,1001))
        self.assertIn('--env-file',m.docker_command(cfg,1001,1001,'caddy','image'))
        upgraded=m.options(['--upgrade'],cfg)
        self.assertEqual(upgraded['tls_dns'],'cloudflare')
        self.assertEqual(upgraded['tls_dns_token_file'],'')
        with self.assertRaises(m.InstallError):
            m.options(['--upgrade','--tls-dns','public'],cfg)

    def test_cloudflare_token_requires_private_regular_file(self):
        with tempfile.TemporaryDirectory() as d:
            source=Path(d)/'token';source.write_text('a'*40);source.chmod(0o600)
            self.assertEqual(m.read_dns_token(source),'a'*40)
            source.chmod(0o644)
            with self.assertRaises(m.InstallError): m.read_dns_token(source)
            source.chmod(0o600);source.write_text('abc\nINJECT=secret')
            with self.assertRaises(m.InstallError): m.read_dns_token(source)
            link=Path(d)/'link';link.symlink_to(source)
            with self.assertRaises(m.InstallError): m.read_dns_token(link)

    def test_actual_backend_listener_must_be_loopback(self):
        from unittest.mock import patch
        port=3030
        def table(address): return f'header\n 0: {address}:0BD6 00000000:0000 0A rest\n'
        with patch.object(m.Path,'read_text',return_value=table('00000000')):
            with self.assertRaises(m.InstallError): m.verify_loopback_backend(port)
        with patch.object(m.Path,'read_text',side_effect=[table('0100007F'),'header\n']):
            m.verify_loopback_backend(port)

    def test_invalid_values_never_enter_units_or_shells(self):
        for args in [['--user','root'],['--user','bad\nUser=root'],['--with','unknown'],['--port','0'],['--version','../../evil'],['--library','relative'],['--alsa-device','x"\nuser "root']]:
            with self.subTest(args=args),self.assertRaises((m.InstallError,SystemExit)): m.options(args)

    def test_checksum_and_archive_rejection(self):
        import hashlib,io,tarfile
        with tempfile.TemporaryDirectory() as d:
            p=Path(d)/'archive.tar.gz'
            with tarfile.open(p,'w:gz') as tf:
                member=tarfile.TarInfo('../outside');member.size=1
                tf.addfile(member,io.BytesIO(b'x'))
            with self.assertRaises(m.InstallError):m.verify_sha256(p,'0'*64)
            m.verify_sha256(p,hashlib.sha256(p.read_bytes()).hexdigest())
            with self.assertRaises(m.InstallError):m.unpack_release(p,Path(d)/'out','musicata-x86_64-linux')


class DeploymentTests(unittest.TestCase):
    def setUp(self):
        from contextlib import ExitStack
        from unittest.mock import patch
        import os
        self.stack=ExitStack();self.addCleanup(self.stack.close)
        self.root=Path(self.stack.enter_context(tempfile.TemporaryDirectory()))
        for name,leaf in [('CONFIG','etc/install.json'),('DATA','state'),('ML_DATA','ml'),('UNIT','etc/musicata.service'),('MPD_UNIT','etc/mpd.service'),('MPD_CONFIG','etc/mpd.conf'),('CURRENT','opt/current'),('BACKUPS','backups'),('NSS','etc/nsswitch.conf'),('AVAHI','etc/avahi.service'),('DSP_CONFIG','etc/camilladsp.json'),('DSP_UNIT','etc/camilladsp.service'),('DSP_MODULE','etc/modules-load.conf'),('CADDY_CONFIG','etc/caddy/Caddyfile'),('CADDY_UNIT','etc/caddy.service'),('CADDY_OVERRIDE','etc/musicata.service.d/caddy.conf'),('CADDY_ROOT','caddy-state'),('CADDY_ENV','etc/caddy/cloudflare.env'),('CADDY_BINARY','opt/caddy')]:
            self.stack.enter_context(patch.object(m,name,self.root/leaf))
        self.artifact=self.root/'artifact';self.artifact.mkdir();(self.artifact/'musicata-server').write_text('fixture binary')
        self.stack.enter_context(patch.object(m,'native_artifact',return_value=self.artifact))
        self.stack.enter_context(patch.object(m,'release_metadata',return_value=('1.0.10',{})))
        self.stack.enter_context(patch.object(m,'install_packages'))
        self.stack.enter_context(patch.object(m,'succeeds',side_effect=lambda a:self.commands.append(list(map(str,a))) or a[0]=='runuser'))
        self.chown=self.stack.enter_context(patch.object(m.os,'chown'))
        self.commands=[]
        self.stack.enter_context(patch.object(m,'run',side_effect=lambda a,**kw:self.commands.append(list(map(str,a))) or ''))
        self.stack.enter_context(patch.object(m.pwd,'getpwnam',return_value=m.pwd.struct_passwd(("musicata","x",os.getuid(),os.getgid(),"","/nonexistent","/usr/sbin/nologin"))))
        self.health=self.stack.enter_context(patch.object(m,'healthy'))
        self.stack.enter_context(patch.object(m,'verify_loopback_backend',create=True))
        self.tls_health=self.stack.enter_context(patch.object(m,'healthy_tls',create=True))
        self.cfg=m.options(['--mode','native','--library',str(self.root/'music')])

    def caddy_options(self, mode='native'):
        return m.options(['--mode',mode,'--with','caddy','--tls-domain','music.example.org','--library',str(self.root/'music')])

    def test_caddy_preflight_rejects_unmanaged_proxy_on_upgrade(self):
        from unittest.mock import patch
        saved,_=self.existing()
        cfg=m.options(['--upgrade','--with','caddy','--tls-domain','music.example.org'],saved)
        m.CADDY_CONFIG.parent.mkdir(parents=True);m.CADDY_CONFIG.write_text('someone else owns this')
        with patch.object(m,'cpu_issues',return_value=[]),patch.object(m.socket,'socket'),patch.object(m,'succeeds',side_effect=lambda a:a==['systemctl','is-active','--quiet','caddy.service']):
            issues=m.preflight(cfg,saved,'debian')
        self.assertTrue(any('Caddy configuration' in item for item in issues),issues)
        self.assertTrue(any('caddy.service' in item for item in issues),issues)
        self.assertEqual(m.CADDY_CONFIG.read_text(),'someone else owns this')

    def test_caddy_upgrade_checks_new_tls_ports(self):
        from unittest.mock import patch
        saved,_=self.existing()
        cfg=m.options(['--upgrade','--with','caddy','--tls-domain','music.example.org'],saved)
        def bind(address):
            if address[1]==443: raise OSError('occupied')
        with patch.object(m,'cpu_issues',return_value=[]),patch.object(m.socket,'socket') as sockets:
            sockets.return_value.__enter__.return_value.bind.side_effect=bind
            issues=m.preflight(cfg,saved,'debian')
        self.assertTrue(any('443' in item for item in issues),issues)

    def test_caddy_preflight_detects_ipv6_only_port_conflict(self):
        from unittest.mock import patch,MagicMock
        saved,_=self.existing()
        cfg=m.options(['--upgrade','--with','caddy','--tls-domain','music.example.org'],saved)
        def sockets(*args):
            sock=MagicMock();sock.__enter__.return_value=sock
            def bind(address):
                if address==('::',443): raise OSError(98,'Address already in use')
            sock.bind.side_effect=bind
            return sock
        with patch.object(m,'cpu_issues',return_value=[]),patch.object(m.socket,'socket',side_effect=sockets):
            issues=m.preflight(cfg,saved,'debian')
        self.assertTrue(any('443 (::)' in item for item in issues),issues)

    def test_native_caddy_install_creates_private_state_and_checks_https(self):
        import json
        cfg=self.caddy_options()
        m.deploy(cfg,None,'debian',[])
        self.assertIn('reverse_proxy 127.0.0.1:3030',m.CADDY_CONFIG.read_text())
        self.assertIn('User=',m.CADDY_UNIT.read_text())
        self.assertIn('CAP_NET_BIND_SERVICE',m.CADDY_UNIT.read_text())
        self.assertIn('MUSICATA_ADDR=127.0.0.1:3030',m.CADDY_OVERRIDE.read_text())
        self.assertEqual(m.CADDY_ROOT.stat().st_mode & 0o777,0o750)
        self.assertEqual(json.loads(m.CONFIG.read_text())['tls_domain'],'music.example.org')
        self.tls_health.assert_called_once_with('music.example.org')
        self.assertIn(['systemctl','restart','musicata-caddy.service'],self.commands)

    def test_caddy_refuses_native_cli_bind_override(self):
        from unittest.mock import patch
        saved,_=self.existing()
        m.UNIT.write_text('[Service]\nExecStart=/opt/musicata/current/musicata-server --addr 0.0.0.0:3030\n')
        cfg=m.options(['--upgrade','--with','caddy','--tls-domain','music.example.org'],saved)
        with patch.object(m,'cpu_issues',return_value=[]),patch.object(m.socket,'socket'):
            self.assertTrue(any('--addr' in issue for issue in m.preflight(cfg,saved,'debian')))

    def test_caddy_migration_revokes_old_plaintext_sessions(self):
        import sqlite3
        m.DATA.mkdir()
        with closing(sqlite3.connect(m.DATA/'musicata.db')) as db, db:
            db.execute('CREATE TABLE sessions (token_hash TEXT)')
            db.execute("INSERT INTO sessions VALUES ('old plaintext session')")
        m.expire_http_sessions()
        with closing(sqlite3.connect(m.DATA/'musicata.db')) as db, db:
            self.assertEqual(db.execute('SELECT count(*) FROM sessions').fetchone()[0],0)

    def test_cloudflare_deploy_preserves_credentials_and_restores_binary_on_failure(self):
        import json
        from unittest.mock import patch
        token=self.root/'token';token.write_text('a'*40);token.chmod(0o600)
        cfg=m.options(['--mode','native','--with','caddy','--tls-domain','music.example.org',
                       '--tls-dns','cloudflare','--tls-dns-token-file',str(token),'--library',str(self.root/'music')])
        binary=self.root/'custom-caddy';binary.write_text('new binary')
        with patch.object(m,'build_caddy',return_value=binary): m.deploy(cfg,None,'debian',[])
        saved=json.loads(m.CONFIG.read_text())
        self.assertNotIn('tls_dns_token_file',saved)
        self.assertNotIn('a'*40,m.CONFIG.read_text())
        self.assertEqual(m.CADDY_ENV.stat().st_mode & 0o777,0o600)
        self.chown.assert_any_call(m.CADDY_CONFIG.parent,0,m.os.getgid())
        token.unlink()
        upgraded=m.options(['--upgrade'],saved)
        binary.write_text('replacement binary')
        self.tls_health.side_effect=m.InstallError('TLS failed')
        with patch.object(m,'build_caddy',return_value=binary):
            with self.assertRaises(m.InstallError): m.deploy(upgraded,saved,'debian',[])
        self.assertEqual(m.CADDY_BINARY.read_text(),'new binary')
        self.assertEqual(m.CADDY_ENV.read_text(),'CLOUDFLARE_API_TOKEN='+'a'*40+'\n')

    def test_native_proxy_uses_separate_caddy_account(self):
        from unittest.mock import patch
        proxy=m.pwd.struct_passwd(('caddy','x',10002,10002,'','/nonexistent','/usr/sbin/nologin'))
        cfg=self.caddy_options()
        with patch.object(m.pwd,'getpwnam',return_value=proxy),patch.object(m.os,'chown') as chown:
            m.configure_caddy(cfg,10001,10001)
        self.assertIn('User=10002',m.CADDY_UNIT.read_text())
        chown.assert_any_call(m.CADDY_ROOT,10002,10002)
        chown.assert_any_call(m.CADDY_CONFIG,0,10002)

    def test_upgrade_adds_caddy_without_replacing_custom_server_unit(self):
        saved,_=self.existing()
        cfg=m.options(['--upgrade','--with','caddy','--tls-domain','music.example.org'],saved)
        m.deploy(cfg,saved,'debian',[])
        self.assertEqual(m.UNIT.read_text(),'custom original unit')
        self.assertIn('MUSICATA_ADDR=127.0.0.1:3030',m.CADDY_OVERRIDE.read_text())

    def test_invalid_caddy_config_is_rejected_before_stopping_server(self):
        from unittest.mock import patch
        saved,_=self.existing()
        cfg=m.options(['--upgrade','--with','caddy','--tls-domain','music.example.org'],saved)
        def invalid(argv,**kw):
            if 'validate' in argv: raise m.InstallError('invalid Caddy configuration')
            self.commands.append(list(map(str,argv)));return ''
        with patch.object(m,'run',side_effect=invalid):
            with self.assertRaisesRegex(m.InstallError,'invalid Caddy'):
                m.deploy(cfg,saved,'debian',[])
        self.assertNotIn(['systemctl','stop','musicata.service'],self.commands)

    def test_failed_native_caddy_upgrade_restores_certificates_and_proxy(self):
        from unittest.mock import patch
        self.cfg=self.caddy_options();saved,old=self.existing()
        m.CADDY_CONFIG.parent.mkdir(parents=True);m.CADDY_CONFIG.write_text('custom proxy')
        m.CADDY_UNIT.write_text('original proxy unit')
        (m.CADDY_ROOT/'data').mkdir(parents=True)
        certificate=m.CADDY_ROOT/'data'/'certificate';certificate.write_text('original certificate')
        def failure(*args):
            certificate.write_text('changed certificate')
            raise m.InstallError('certificate validation failed')
        self.tls_health.side_effect=failure
        with patch.object(m,'succeeds',side_effect=lambda a:a[0]=='runuser' or a==['systemctl','is-active','--quiet','musicata-caddy.service']):
            with self.assertRaisesRegex(m.InstallError,'certificate validation'):
                m.deploy(self.cfg,saved,'debian',[])
        self.assertEqual(certificate.read_text(),'original certificate')
        self.assertEqual(m.CADDY_CONFIG.read_text(),'custom proxy')
        self.assertEqual(m.CADDY_UNIT.read_text(),'original proxy unit')
        self.assertEqual(m.CURRENT.resolve(),old)
        self.assertIn(['systemctl','start','musicata-caddy.service'],self.commands)

    def test_fresh_caddy_tls_failure_removes_new_proxy_files(self):
        self.tls_health.side_effect=m.InstallError('certificate unavailable')
        with self.assertRaisesRegex(m.InstallError,'certificate unavailable'):
            m.deploy(self.caddy_options(),None,'debian',[])
        self.assertFalse(m.CADDY_CONFIG.exists())
        self.assertFalse(m.CADDY_UNIT.exists())
        self.assertFalse(m.CADDY_OVERRIDE.exists())
        self.assertFalse(m.CONFIG.exists())
        self.assertIn(['systemctl','disable','musicata-caddy.service'],self.commands)

    def test_docker_caddy_upgrade_recovers_both_containers_on_tls_failure(self):
        from unittest.mock import patch
        self.cfg=self.caddy_options('docker');saved,_=self.existing()
        containers={'musicata':'old-server','musicata-caddy':'old-proxy'}
        def docker_run(argv,**kw):
            argv=list(map(str,argv));self.commands.append(argv)
            if argv[:2]==['docker','rename']: containers[argv[3]]=containers.pop(argv[2])
            if argv[:2]==['docker','run'] and '--name' in argv: containers[argv[argv.index('--name')+1]]=argv[-1]
            return ''
        def ok(argv):
            if argv[:3]==['docker','rm','-f']: containers.pop(argv[3],None)
            return True
        self.tls_health.side_effect=m.InstallError('TLS failed')
        with patch.object(m,'run',side_effect=docker_run),patch.object(m,'succeeds',side_effect=ok),patch.object(m,'docker_images',return_value={'server':'new-server','caddy':'new-proxy'}):
            with self.assertRaisesRegex(m.InstallError,'TLS failed'):m.deploy(self.cfg,saved,'debian',[])
        self.assertEqual(containers,{'musicata':'old-server','musicata-caddy':'old-proxy'})
        self.assertIn(['docker','start','musicata-caddy'],self.commands)

    def existing(self):
        import json
        import sqlite3
        m.DATA.mkdir()
        with closing(sqlite3.connect(m.DATA/'musicata.db')) as db, db:
            db.execute('CREATE TABLE sessions (token_hash TEXT)')
            db.execute("INSERT INTO sessions VALUES ('old')")
        self.original_database=(m.DATA/'musicata.db').read_bytes()
        m.UNIT.parent.mkdir(parents=True);m.UNIT.write_text('custom original unit')
        m.CURRENT.parent.mkdir();old=m.CURRENT.parent/'old';old.mkdir();m.CURRENT.symlink_to(old)
        saved={k:v for k,v in self.cfg.items() if k not in ('upgrade','yes','check','dry_run')}
        m.CONFIG.write_text(json.dumps(saved));return saved,old

    def test_dsp_configuration_targets_dac_and_failure_restores_files(self):
        from unittest.mock import patch
        cfg=m.options(['--mode','native','--with','mpd,dsp','--alsa-device','hw:CARD=USB,DEV=0','--library',str(self.root/'music')])
        saved,old=self.existing()
        m.DSP_CONFIG.write_text('original processor routing')
        m.DSP_UNIT.write_text('original processor unit')
        m.MPD_CONFIG.write_text('original MPD routing')
        self.health.side_effect=m.InstallError('health failed')
        with patch.object(m.shutil,'which',return_value='/usr/bin/camilladsp'),patch.object(m,'configure_mpd'),patch.object(m,'succeeds',side_effect=lambda a:a[0]=='runuser' or a==['systemctl','is-active','--quiet','musicata-camilladsp.service']):
            with self.assertRaises(m.InstallError): m.deploy(cfg,saved,'debian',[])
        self.assertEqual(m.DSP_CONFIG.read_text(),'original processor routing')
        self.assertEqual(m.DSP_UNIT.read_text(),'original processor unit')
        self.assertEqual(m.MPD_CONFIG.read_text(),'original MPD routing')
        self.assertFalse(m.DSP_MODULE.exists())
        self.assertEqual(m.CURRENT.resolve(),old)
        self.assertIn(['systemctl','start','musicata.service'],self.commands)
        self.assertIn(['modprobe','snd-aloop'],self.commands)
        self.assertIn(['systemctl','start','musicata-camilladsp.service'],self.commands)

    def test_failed_fresh_dsp_install_unloads_only_a_new_loopback_module(self):
        from unittest.mock import patch
        cfg=m.options(['--mode','native','--with','mpd,dsp','--alsa-device','hw:CARD=USB,DEV=0','--library',str(self.root/'music')])
        self.health.side_effect=m.InstallError('health failed')
        exists = Path.exists
        with patch.object(m.shutil,'which',return_value='/usr/bin/camilladsp'),patch.object(m,'configure_mpd'),patch.object(m.Path,'exists',autospec=True,side_effect=lambda p: False if str(p)=='/sys/module/snd_aloop' else exists(p)):
            with self.assertRaises(m.InstallError): m.deploy(cfg,None,'debian',[])
        self.assertIn(['modprobe','-r','snd-aloop'],self.commands)

    def test_failed_dsp_install_preserves_an_existing_loopback_module(self):
        from unittest.mock import patch
        cfg=m.options(['--mode','native','--with','mpd,dsp','--alsa-device','hw:CARD=USB,DEV=0','--library',str(self.root/'music')])
        self.health.side_effect=m.InstallError('health failed')
        exists = Path.exists
        with patch.object(m.shutil,'which',return_value='/usr/bin/camilladsp'),patch.object(m,'configure_mpd'),patch.object(m.Path,'exists',autospec=True,side_effect=lambda p: True if str(p)=='/sys/module/snd_aloop' else exists(p)):
            with self.assertRaises(m.InstallError): m.deploy(cfg,None,'debian',[])
        self.assertNotIn(['modprobe','-r','snd-aloop'],self.commands)

    def test_dsp_service_and_config_are_prepared_without_device_access(self):
        from unittest.mock import patch
        cfg=m.options(['--mode','native','--with','mpd,dsp','--alsa-device','hw:CARD=USB,DEV=0'])
        with patch.object(m.shutil,'which',return_value='/usr/bin/camilladsp'):
            m.configure_dsp(cfg,1000,1000)
        import json
        self.assertEqual(json.loads(m.DSP_CONFIG.read_text())['devices']['playback']['device'],'hw:CARD=USB,DEV=0')
        self.assertIn('127.0.0.1 -p 1234',m.DSP_UNIT.read_text())
        self.assertEqual(m.DSP_MODULE.read_text(),'snd-aloop\n')

    def test_fresh_install_creates_non_root_service_and_saved_settings(self):
        import json
        m.deploy(self.cfg,None,'debian',[])
        self.assertEqual(json.loads(m.CONFIG.read_text())['version'],'1.0.10')
        self.assertIn('NoNewPrivileges=true',m.UNIT.read_text())
        self.assertTrue((m.CURRENT/'musicata-server').is_file())

    def test_upgrade_preserves_data_and_custom_unit(self):
        saved,old=self.existing()
        diagnostics=m.DATA/'musicata.db.diagnostics.db'
        diagnostics.write_bytes(b'private operational evidence')
        m.deploy(self.cfg,saved,'debian',[])
        self.assertEqual(diagnostics.read_bytes(),b'private operational evidence')
        self.assertEqual((m.DATA/'musicata.db').read_bytes(),self.original_database)
        self.assertEqual(m.UNIT.read_text(),'custom original unit')
        self.assertNotEqual(m.CURRENT.resolve(),old)
        self.assertTrue(list(m.BACKUPS.glob('*/state.tar.gz')))

    def test_failed_upgrade_restores_database_binary_and_unit(self):
        saved,old=self.existing()
        def fail(*a):
            (m.DATA/'musicata.db').write_text('new schema')
            raise m.InstallError('failed health check')
        self.health.side_effect=fail
        with self.assertRaises(m.InstallError):m.deploy(self.cfg,saved,'debian',[])
        self.assertEqual((m.DATA/'musicata.db').read_bytes(),self.original_database)
        self.assertEqual(m.CURRENT.resolve(),old)
        self.assertEqual(m.UNIT.read_text(),'custom original unit')
        self.assertIn(['systemctl','start','musicata.service'],self.commands)

    def test_failed_first_install_can_be_retried(self):
        self.health.side_effect=m.InstallError('failed health check')
        with self.assertRaises(m.InstallError):m.deploy(self.cfg,None,'debian',[])
        self.assertFalse(m.UNIT.exists())
        self.assertFalse(m.CURRENT.is_symlink())
        self.assertFalse(m.CONFIG.exists())
        self.assertFalse(any(m.DATA.iterdir()))

    def test_restore_failure_still_restores_binary_and_reports_manual_recovery(self):
        from unittest.mock import patch
        saved,old=self.existing()
        self.health.side_effect=m.InstallError('new service failed')
        with patch.object(m.tarfile.TarFile,'extractall',side_effect=OSError('disk failure')):
            with self.assertRaisesRegex(m.InstallError,'manual recovery'):
                m.deploy(self.cfg,saved,'debian',[])
        self.assertEqual(m.CURRENT.resolve(),old)
        self.assertNotIn(['systemctl','start','musicata.service'],self.commands)

    def test_docker_upgrade_restores_previous_containers_on_health_failure(self):
        from unittest.mock import patch
        self.cfg['mode']='docker'
        saved,_=self.existing()
        containers={'musicata':'old-image'}
        def docker_run(argv,**kw):
            argv=list(map(str,argv));self.commands.append(argv)
            if argv[:2]==['docker','rename']: containers[argv[3]]=containers.pop(argv[2])
            if argv[:2]==['docker','run']: containers[argv[argv.index('--name')+1]]=argv[-1]
            return ''
        def ok(argv):
            if argv[:3]==['docker','rm','-f']: containers.pop(argv[3],None)
            return True
        def unhealthy(*args):
            (m.DATA/'musicata.db').write_text('new schema')
            raise m.InstallError('health failed')
        self.health.side_effect=unhealthy
        with patch.object(m,'run',side_effect=docker_run),patch.object(m,'succeeds',side_effect=ok),patch.object(m,'docker_images',return_value={'server':'new-image'}):
            with self.assertRaises(m.InstallError):m.deploy(self.cfg,saved,'debian',[])
        self.assertEqual(containers,{'musicata':'old-image'})
        self.assertEqual((m.DATA/'musicata.db').read_bytes(),self.original_database)
        self.assertIn(['docker','start','musicata'],self.commands)

    def test_failed_image_preparation_does_not_stop_existing_server(self):
        from unittest.mock import patch
        self.cfg['mode']='docker';saved,_=self.existing()
        with patch.object(m,'docker_images',side_effect=m.InstallError('unauthorized')):
            with self.assertRaisesRegex(m.InstallError,'unauthorized'):m.deploy(self.cfg,saved,'debian',[])
        self.assertFalse(any(c[:2]==['docker','stop'] for c in self.commands))
        self.assertEqual((m.DATA/'musicata.db').read_bytes(),self.original_database)

    def test_docker_runtime_uses_nonroot_persistent_state_and_loopback_ml(self):
        cfg=m.options(['--with','ml,mpd'])
        server=m.docker_command(cfg,1001,1001,'server','sha256:test')
        ml=m.docker_command(cfg,1001,1001,'ml','sha256:ml')
        self.assertIn('1001:1001',server)
        self.assertIn(str(m.DATA)+':/data:Z',server)
        self.assertIn('/srv/music:/music:ro,z',server)
        self.assertIn('MUSICATA_MPD=127.0.0.1:6600',server)
        self.assertIn('MUSICATA_ML_ADDR=127.0.0.1:3091',ml)
        self.assertNotIn('--device',ml)

if __name__=='__main__': unittest.main()
