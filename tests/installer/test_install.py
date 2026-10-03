import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location('musicata_install', Path(__file__).resolve().parents[2] / 'packaging/install.py')
m = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = m
spec.loader.exec_module(m)

class PlanningTests(unittest.TestCase):
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

    def test_package_selection(self):
        for family, engine in [('debian','docker.io'),('arch','docker'),('fedora','moby-engine')]:
            self.assertIn(engine,m.packages(family,m.options([])))
            self.assertIn('mpd',m.packages(family,m.options(['--mode','native','--with','mpd'])))
        self.assertIn('snapcast',m.packages('arch',m.options(['--mode','native','--with','snapcast'])))

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
        from types import SimpleNamespace
        import os
        self.stack=ExitStack();self.addCleanup(self.stack.close)
        self.root=Path(self.stack.enter_context(tempfile.TemporaryDirectory()))
        for name,leaf in [('CONFIG','etc/install.json'),('DATA','state'),('ML_DATA','ml'),('UNIT','etc/musicata.service'),('MPD_UNIT','etc/mpd.service'),('MPD_CONFIG','etc/mpd.conf'),('CURRENT','opt/current'),('BACKUPS','backups'),('NSS','etc/nsswitch.conf'),('AVAHI','etc/avahi.service')]:
            self.stack.enter_context(patch.object(m,name,self.root/leaf))
        self.artifact=self.root/'artifact';self.artifact.mkdir();(self.artifact/'musicata-server').write_text('fixture binary')
        self.stack.enter_context(patch.object(m,'native_artifact',return_value=self.artifact))
        self.stack.enter_context(patch.object(m,'release_metadata',return_value=('1.0.10',{})))
        self.stack.enter_context(patch.object(m,'install_packages'))
        self.stack.enter_context(patch.object(m,'succeeds',side_effect=lambda a:a[0]=='runuser'))
        self.commands=[]
        self.stack.enter_context(patch.object(m,'run',side_effect=lambda a,**kw:self.commands.append(list(map(str,a))) or ''))
        self.stack.enter_context(patch.object(m.pwd,'getpwnam',return_value=SimpleNamespace(pw_uid=os.getuid(),pw_gid=os.getgid())))
        self.health=self.stack.enter_context(patch.object(m,'healthy'))
        self.cfg=m.options(['--mode','native','--library',str(self.root/'music')])

    def existing(self):
        import json
        m.DATA.mkdir();(m.DATA/'musicata.db').write_text('original database')
        m.UNIT.parent.mkdir(parents=True);m.UNIT.write_text('custom original unit')
        m.CURRENT.parent.mkdir();old=m.CURRENT.parent/'old';old.mkdir();m.CURRENT.symlink_to(old)
        saved={k:v for k,v in self.cfg.items() if k not in ('upgrade','yes','check','dry_run')}
        m.CONFIG.write_text(json.dumps(saved));return saved,old

    def test_fresh_install_creates_non_root_service_and_saved_settings(self):
        import json
        m.deploy(self.cfg,None,'debian',[])
        self.assertEqual(json.loads(m.CONFIG.read_text())['version'],'1.0.10')
        self.assertIn('NoNewPrivileges=true',m.UNIT.read_text())
        self.assertTrue((m.CURRENT/'musicata-server').is_file())

    def test_upgrade_preserves_data_and_custom_unit(self):
        saved,old=self.existing()
        m.deploy(self.cfg,saved,'debian',[])
        self.assertEqual((m.DATA/'musicata.db').read_text(),'original database')
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
        self.assertEqual((m.DATA/'musicata.db').read_text(),'original database')
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
        self.assertEqual((m.DATA/'musicata.db').read_text(),'original database')
        self.assertIn(['docker','start','musicata'],self.commands)

    def test_failed_image_preparation_does_not_stop_existing_server(self):
        from unittest.mock import patch
        self.cfg['mode']='docker';saved,_=self.existing()
        with patch.object(m,'docker_images',side_effect=m.InstallError('unauthorized')):
            with self.assertRaisesRegex(m.InstallError,'unauthorized'):m.deploy(self.cfg,saved,'debian',[])
        self.assertFalse(any(c[:2]==['docker','stop'] for c in self.commands))
        self.assertEqual((m.DATA/'musicata.db').read_text(),'original database')

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
