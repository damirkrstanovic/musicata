"""Player provisioning tests use temporary files and simulated service control."""
import importlib.util
from pathlib import Path
import sys
import tempfile
import unittest

spec = importlib.util.spec_from_file_location(
    'musicata_player', Path(__file__).resolve().parents[2] / 'packaging/player.py')
player = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = player
spec.loader.exec_module(player)


class OutputTests(unittest.TestCase):
    def test_outputs_can_be_discovered_before_alsa_utils_is_installed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            card = root / 'card4'
            (card / 'pcm0p').mkdir(parents=True)
            (card / 'id').write_text('Pro\n')
            (card / 'pcm0p/info').write_text('name: USB Audio\n')
            self.assertEqual(player.proc_outputs(root),
                             [{'device': 'plughw:CARD=Pro,DEV=0', 'name': 'Pro: USB Audio'}])

    def test_mpd_snapcast_transport_never_claims_alsa_device(self):
        config = player.mpd_config('plughw:CARD=Pro,DEV=0', fifo='/run/musicata/mpd.fifo')
        self.assertIn('type "fifo"', config)
        self.assertIn('format "48000:16:2"', config)
        self.assertNotIn('type "alsa"', config)
        self.assertNotIn('plughw', config)

    def test_snapcast_transport_cannot_silently_bypass_existing_dsp(self):
        with self.assertRaises(ValueError):
            player.mpd_config('plughw:CARD=Pro,DEV=0', dsp=True, fifo='/run/musicata/mpd.fifo')

    def test_stable_identifiers_survive_card_renumbering(self):
        first = 'card 0: Pro [USB DAC], device 0: USB Audio [USB Audio]\n'
        second = first.replace('card 0:', 'card 4:')
        self.assertEqual(player.parse_outputs(first), player.parse_outputs(second))
        self.assertEqual(player.parse_outputs(first)[0]['device'], 'plughw:CARD=Pro,DEV=0')

    def test_distinct_outputs_remain_distinct(self):
        text = ('card 0: HDMI [HDMI], device 3: HDMI 0 [HDMI 0]\n'
                'card 1: Pro [USB DAC], device 0: USB Audio [USB Audio]\n')
        self.assertEqual(len(player.parse_outputs(text)), 2)

    def test_no_devices_never_falls_back_to_default(self):
        self.assertEqual(player.parse_outputs('aplay: no soundcards found...'), [])

    def test_unit_rejects_injected_systemd_directives(self):
        with self.assertRaises(ValueError):
            player.snapclient_unit(1000, 1000, 'plughw:CARD=Pro,DEV=0',
                                   'host\nExecStart=/bin/false', 'identity')

    def test_snapclient_retries_missing_dac_and_preserves_identity(self):
        unit = player.snapclient_unit(1000, 1000, 'plughw:CARD=Pro,DEV=0',
                                     'server.local', 'stable-player')
        self.assertIn('RestartSec=5', unit)
        self.assertIn('--hostID stable-player', unit)
        self.assertIn('User=1000', unit)
        self.assertIn('--host server.local', unit)


class TransactionTests(unittest.TestCase):
    def test_failed_hardware_restore_retains_current_binding_for_recovery(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config = root / 'binding.json'
            config.write_text('old port')
            commands = []
            def unhealthy():
                raise RuntimeError('protocol unavailable')
            def restore_port():
                self.assertEqual(config.read_text(), 'new port')
                self.assertEqual(commands[-1], ['systemctl', 'stop', 'usb.service'])
                raise RuntimeError('port unavailable')
            with self.assertRaisesRegex(RuntimeError, 'current binding retained'):
                player.apply_files({config: ('new port', 0o600)}, 'usb.service', root / 'backup',
                                   commands.append, lambda _: False, unhealthy, before_restore=restore_port)
            self.assertEqual(config.read_text(), 'new port')
            self.assertEqual((root / 'backup/0').read_text(), 'old port')

    def test_failed_activation_restores_file_and_exact_service_state(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config = root / 'player.conf'
            config.write_text('custom original')
            config.chmod(0o640)
            commands = []

            def run(command):
                commands.append(command)
                if command == ['systemctl', 'restart', 'musicata-mpd.service']:
                    raise RuntimeError('activation failed')

            with self.assertRaisesRegex(RuntimeError, 'activation failed'):
                player.apply_files({config: ('replacement', 0o600)},
                                   'musicata-mpd.service', root / 'backup', run,
                                   lambda command: 'is-active' in command,
                                   lambda: None)
            self.assertEqual(config.read_text(), 'custom original')
            self.assertEqual(config.stat().st_mode & 0o777, 0o640)
            self.assertIn(['systemctl', 'disable', 'musicata-mpd.service'], commands)
            self.assertIn(['systemctl', 'start', 'musicata-mpd.service'], commands)

    def test_failed_fresh_install_removes_only_new_managed_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config = root / 'player.conf'
            unrelated = root / 'other.conf'
            unrelated.write_text('preserve')
            commands = []

            def unhealthy():
                raise RuntimeError('protocol unavailable')

            with self.assertRaisesRegex(RuntimeError, 'protocol unavailable'):
                player.apply_files({config: ('new', 0o600)}, 'musicata-mpd.service',
                                   root / 'backup', commands.append, lambda _: False, unhealthy)
            self.assertFalse(config.exists())
            self.assertEqual(unrelated.read_text(), 'preserve')
            self.assertIn(['systemctl', 'stop', 'musicata-mpd.service'], commands)
