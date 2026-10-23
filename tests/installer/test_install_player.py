import importlib.util
from pathlib import Path
import sys
import unittest
import tempfile
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'packaging'))
spec = importlib.util.spec_from_file_location('musicata_install_player',
    Path(__file__).resolve().parents[2] / 'packaging/install_player.py')
entry = importlib.util.module_from_spec(spec)
spec.loader.exec_module(entry)


class StandalonePlanningTests(unittest.TestCase):
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
