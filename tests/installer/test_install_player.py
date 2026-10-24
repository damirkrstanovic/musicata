import importlib.util
from pathlib import Path
import sys
import unittest
import tempfile
import json
from types import SimpleNamespace
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import threading
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'packaging'))
spec = importlib.util.spec_from_file_location('musicata_install_player',
    Path(__file__).resolve().parents[2] / 'packaging/install_player.py')
entry = importlib.util.module_from_spec(spec)
spec.loader.exec_module(entry)


class StandalonePlanningTests(unittest.TestCase):
    def test_mpd_packages_do_not_install_snapcast(self):
        for family in ('debian', 'fedora', 'arch'):
            packages = entry.dependencies(family, 'mpd')
            self.assertIn('mpd', packages)
            self.assertNotIn('snapclient', packages)
            self.assertNotIn('cmake', packages)

    def test_mpd_network_binding_requires_explicit_trust(self):
        with self.assertRaisesRegex(ValueError, 'trusted'):
            entry.options(['--backend', 'mpd', '--mpd-bind', '192.168.1.20'])
        cfg = entry.options(['--backend', 'mpd', '--mpd-bind', '192.168.1.20', '--confirm-trusted-network'])
        self.assertEqual(cfg['mpd_bind'], '192.168.1.20')
        for address in ('0.0.0.0', '8.8.8.8', 'host.local'):
            with self.assertRaises(ValueError):
                entry.options(['--backend', 'mpd', '--mpd-bind', address, '--confirm-trusted-network'])

    def test_direct_mpd_rejects_usb_automation(self):
        with self.assertRaisesRegex(ValueError, 'USB'):
            entry.options(['--backend', 'mpd', '--usb-device', '/sys/devices/dac'])

    def test_remote_musicata_requires_reachable_bind_before_install(self):
        args = ['--backend', 'mpd', '--musicata-url', 'https://musicata.example.org']
        with self.assertRaisesRegex(ValueError, 'remote Musicata'):
            entry.options(args)
        cfg = entry.options(args + ['--mpd-bind', '192.168.1.20', '--confirm-trusted-network'])
        self.assertEqual(cfg['mpd_bind'], '192.168.1.20')

    def test_backend_migration_requires_explicit_migration(self):
        with self.assertRaisesRegex(ValueError, 'migration'):
            entry.options(['--backend', 'mpd'], {'backend': 'snapclient', 'user': 'musicata'})

    def test_mpd_render_uses_shared_output_and_private_bind(self):
        cfg = entry.options(['--backend', 'mpd', '--output', 'plughw:CARD=Pro,DEV=0'])
        files = entry.playback_files(cfg, 1000, 1000)
        config = files[entry.MPD_CONFIG][0]
        self.assertIn('bind_to_address "127.0.0.1"', config)
        self.assertIn('device "plughw:CARD=Pro,DEV=0"', config)
        self.assertIn('User=1000', files[entry.MPD_UNIT][0])

    def test_player_packages_never_include_snapserver(self):
        for family in ('debian', 'fedora', 'arch'):
            with self.subTest(family=family):
                packages = entry.dependencies(family)
                self.assertNotIn('snapserver', packages)
                self.assertNotIn('mpd', packages)
                self.assertNotIn('shairport-sync', packages)

    def test_saved_host_identity_survives_server_reconfiguration(self):
        saved = {'server': 'old.local', 'output': 'plughw:CARD=Pro,DEV=0',
                 'identity': 'same-player', 'user': 'musicata'}
        options = entry.options(['--server', 'new.local'], saved)
        self.assertEqual(options['identity'], 'same-player')
        self.assertEqual(options['server'], 'new.local')
        self.assertEqual(options['output'], saved['output'])

    def test_options_reject_systemd_injection(self):
        with self.assertRaises(ValueError):
            entry.options(['--server', 'host\nExecStart=/bin/false'])

    def test_generic_linux_requires_existing_binary(self):
        from unittest.mock import patch
        with patch.object(entry.shutil, 'which', return_value=None):
            with self.assertRaisesRegex(entry.installer.InstallError, 'existing Snapclient'):
                entry.generic_command({'server': 'host', 'output': 'plughw:CARD=Pro,DEV=0',
                                       'identity': 'player'})

    def test_generic_launch_command_uses_selected_server_and_output(self):
        from unittest.mock import patch
        with patch.object(entry.shutil, 'which', return_value='/usr/bin/snapclient'):
            command = entry.generic_command({'server': 'host', 'output': 'plughw:CARD=Pro,DEV=0',
                                            'identity': 'player'})
        self.assertIn('--host host', command)
        self.assertIn('--hostID player', command)
        self.assertIn('--soundcard plughw:CARD=Pro,DEV=0', command)


class MpdInstallationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        for name in ('CONFIG', 'UNIT', 'MPD_UNIT', 'MPD_CONFIG', 'MPD_ROOT', 'USB_CONFIG', 'LOCK', 'BACKUPS'):
            self.patch(entry, name, root / name)
        original_is_dir = Path.is_dir
        self.patch(Path, 'is_dir', lambda path: True if str(path) == '/run/systemd/system' else original_is_dir(path))
        self.patch(entry.sys.stdin, 'isatty', return_value=False)
        self.patch(entry.os, 'geteuid', return_value=0)
        self.patch(entry.os, 'chown')
        self.patch(entry.pwd, 'getpwnam', return_value=SimpleNamespace(pw_uid=123, pw_gid=123))
        self.patch(entry, 'private_read', return_value='test-secret')
        self.patch(entry.installer, 'succeeds', return_value=False)
        self.patch(entry.installer, 'package_installed', return_value=True)
        self.patch(entry.installer, 'install_packages')
        self.run = self.patch(entry.installer, 'run', return_value='card 1: Pro [DAC], device 0: USB Audio')
        self.verify = self.patch(entry, 'verify')
        self.request = self.patch(entry, 'registration_request', return_value=[])
        self.register = self.patch(entry, 'register_mpd')
        self.args = ['--backend', 'mpd', '--output', 'plughw:CARD=Pro,DEV=0',
                     '--musicata-url', 'http://localhost:3030', '--token-file', '/private/token', '--yes']

    def patch(self, obj, name, *args, **kwargs):
        patcher = patch.object(obj, name, *args, **kwargs)
        value = patcher.start()
        self.addCleanup(patcher.stop)
        return value

    def test_install_and_rerun_preserve_state_without_saving_credentials(self):
        self.assertEqual(entry.main(self.args), 0)
        state = entry.MPD_ROOT / 'state'
        state.write_text('existing queue')
        self.assertEqual(entry.main(self.args), 0)
        self.assertEqual(state.read_text(), 'existing queue')
        manifest = json.loads(entry.CONFIG.read_text())
        self.assertEqual(manifest['backend'], 'mpd')
        self.assertNotIn('token_file', manifest)
        self.assertNotIn('test-secret', entry.CONFIG.read_text())
        self.assertFalse(entry.UNIT.exists())
        self.assertEqual(self.register.call_count, 2)

    def test_custom_config_is_not_overwritten(self):
        entry.main(self.args)
        entry.MPD_CONFIG.write_text('custom MPD configuration')
        with self.assertRaisesRegex(entry.installer.InstallError, 'customized'):
            entry.main(self.args)
        self.assertEqual(entry.MPD_CONFIG.read_text(), 'custom MPD configuration')

    def test_failed_activation_restores_prior_config(self):
        entry.main(self.args)
        previous = entry.MPD_CONFIG.read_text()
        self.verify.side_effect = RuntimeError('activation failed')
        with self.assertRaisesRegex(RuntimeError, 'activation failed'):
            entry.main(self.args)
        self.assertEqual(entry.MPD_CONFIG.read_text(), previous)

    def test_registration_failure_retains_usable_local_install(self):
        self.register.side_effect = OSError('server unavailable')
        with self.assertRaisesRegex(entry.installer.InstallError, 'local setup is retained'):
            entry.main(self.args)
        self.assertTrue(entry.MPD_UNIT.exists())
        self.assertTrue(entry.CONFIG.exists())


class MpdRegistrationTests(unittest.TestCase):
    def test_registration_uses_bearer_header_and_json_post(self):
        seen = []
        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                seen.append((self.path, self.headers.get('Authorization'),
                             json.loads(self.rfile.read(int(self.headers['Content-Length'])))))
                self.send_response(200)
                self.end_headers()
                self.wfile.write(b'{"id":"registered"}')
            def log_message(self, *args):
                pass
        server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            cfg = {'musicata_url': 'http://127.0.0.1:' + str(server.server_port)}
            result = entry.registration_request(cfg, 'secret', {'kind': 'mpd', 'address': '127.0.0.1:6600'})
            self.assertEqual(result['id'], 'registered')
            self.assertEqual(seen, [('/api/players', 'Bearer secret', {'kind': 'mpd', 'address': '127.0.0.1:6600'})])
        finally:
            server.shutdown()
            server.server_close()
            thread.join()

    def test_existing_address_is_not_registered_twice(self):
        existing = {'id': 'same', 'kind': 'mpd', 'address': '127.0.0.1:6600', 'online': True}
        with patch.object(entry, 'registration_request', return_value=[existing]) as request:
            entry.register_mpd({'mpd_bind': '127.0.0.1'}, 'secret')
        self.assertTrue(all(len(call.args) == 2 for call in request.call_args_list))

    def test_registration_waits_for_server_to_observe_connection(self):
        item = {'id': 'new', 'online': True}
        with patch.object(entry, 'registration_request', side_effect=[[], item, [item]]) as request:
            entry.register_mpd({'mpd_bind': '192.168.1.2'}, 'secret')
        self.assertEqual(request.call_args_list[1].args[2]['address'], '192.168.1.2:6600')


class UsbLifecycleTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        for name in ('USB_CONFIG', 'USB_CODE', 'USB_UNIT', 'USB_TOKEN'):
            path = self.root / name
            path.write_text('old')
            patcher = patch.object(entry, name, path)
            patcher.start()
            self.addCleanup(patcher.stop)

    def test_rebind_restores_old_port_before_replacing_config(self):
        disabled = [True]
        def run(command):
            if command[-1] == '--restore':
                self.assertEqual(entry.USB_CONFIG.read_text(), 'old')
                disabled[0] = False
        def apply(*args, **kwargs):
            self.assertFalse(disabled[0])
            entry.USB_CONFIG.write_text('new')
        with patch.object(entry.installer, 'run', side_effect=run), patch.object(entry.installer, 'succeeds', return_value=True), patch.object(entry.player, 'apply_files', side_effect=apply):
            entry.apply_usb({}, self.root / 'backup')
        self.assertEqual(entry.USB_CONFIG.read_text(), 'new')

    def test_failed_restore_preserves_binding_and_does_not_apply(self):
        with patch.object(entry.installer, 'succeeds', return_value=False), patch.object(entry.installer, 'run', side_effect=OSError('unavailable')), patch.object(entry.player, 'apply_files') as apply:
            with self.assertRaises(OSError):
                entry.apply_usb({}, self.root / 'backup')
            apply.assert_not_called()
        self.assertEqual(entry.USB_CONFIG.read_text(), 'old')

    def test_removal_restores_before_disabling_and_deleting(self):
        calls = []
        def run(command):
            calls.append(command)
            if command[-1] == '--restore':
                self.assertTrue(entry.USB_CONFIG.exists())
        with patch.object(entry.installer, 'succeeds', return_value=True), patch.object(entry.installer, 'run', side_effect=run):
            entry.remove_usb()
        self.assertEqual(calls[0][1], 'stop')
        self.assertEqual(calls[1][-1], '--restore')
        self.assertEqual(calls[2][1], 'disable')
        self.assertFalse(entry.USB_CONFIG.exists())

    def test_missing_unit_still_restores_port(self):
        entry.USB_UNIT.unlink()
        with patch.object(entry.installer, 'succeeds', return_value=False), patch.object(entry.installer, 'run') as run:
            entry.remove_usb()
        self.assertEqual(run.call_args_list[0].args[0][-1], '--restore')

    def test_failed_removal_keeps_all_recovery_files(self):
        with patch.object(entry.installer, 'succeeds', return_value=False), patch.object(entry.installer, 'run', side_effect=OSError('unavailable')):
            with self.assertRaises(OSError):
                entry.remove_usb()
        for path in (entry.USB_CONFIG, entry.USB_CODE, entry.USB_UNIT, entry.USB_TOKEN):
            self.assertTrue(path.exists())
