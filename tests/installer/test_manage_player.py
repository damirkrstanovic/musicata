# SPDX-License-Identifier: AGPL-3.0-or-later
import json
import os
from pathlib import Path
import stat
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'packaging'))
import manage_player as management

class InventoryTests(unittest.TestCase):
    def test_hardware_identity_survives_card_number_and_alsa_name_changes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            proc, sysfs = root/'proc', root/'sys'
            hardware = root/'devices'/'usb1'/'1-2'
            hardware.mkdir(parents=True)
            for name, value in [('serial','DAC123'),('idVendor','1234'),('idProduct','5678')]:
                (hardware/name).write_text(value)
            def discover(number, card):
                (proc/f'card{number}').mkdir(parents=True)
                (proc/f'card{number}'/'id').write_text(card)
                (sysfs/f'card{number}').mkdir(parents=True)
                (sysfs/f'card{number}'/'device').symlink_to(hardware)
                return management.inventory_devices([{'device':f'plughw:CARD={card},DEV=0','name':'USB DAC'}],proc,sysfs)
            before=discover(2,'Pro')
            self.assertEqual(len(before),1)
            after=discover(7,'Pro_1')
            self.assertEqual(before[0]['id'],after[0]['id'])
            self.assertNotEqual(before[0]['route'],after[0]['route'])

    def test_unknown_hardware_is_not_assigned_a_guessed_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary)
            self.assertEqual(management.inventory_devices([{'device':'plughw:CARD=Missing,DEV=0','name':'Missing'}],root,root),[])

    def test_platform_output_identity_survives_card_renumbering(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            proc, sysfs = root/'proc', root/'sys'
            hardware = root/'devices'/'pci0000:00'/'0000:00:1f.3'/'sound'
            hardware.mkdir(parents=True)
            def discover(number, card):
                (proc/f'card{number}').mkdir(parents=True)
                (proc/f'card{number}'/'id').write_text(card)
                (sysfs/f'card{number}').mkdir(parents=True)
                target = hardware / f'card{number}'
                target.mkdir()
                (sysfs/f'card{number}'/'device').symlink_to(target)
                return management.inventory_devices([{'device':f'plughw:CARD={card},DEV=3','name':'HDMI'}],proc,sysfs)
            self.assertEqual(discover(0, 'HDMI')[0]['id'], discover(5, 'HDMI_1')[0]['id'])


class ManagedApplyTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.config = self.root / 'etc/player.json'
        self.unit = self.root / 'etc/musicata-snapclient.service'
        self.state = self.root / 'state/state.json'
        self.backups = self.root / 'backups'
        self.lock = self.root / 'lock'
        self.cfg = {'backend': 'snapclient', 'user': 'musicata', 'server': 'server',
                    'identity': 'client', 'output': 'plughw:CARD=Pro,DEV=0'}
        self.unit_text = ('[Service]\nExecStart=/usr/bin/snapclient --host server --hostID client '
                          '--soundcard plughw:CARD=Pro,DEV=0\n')
        self.cfg['unit_digest'] = management.digest(self.unit_text.encode())
        self.config.parent.mkdir(parents=True)
        self.config.write_text(json.dumps(self.cfg))
        self.config.chmod(0o600)
        self.unit.parent.mkdir(parents=True, exist_ok=True)
        self.unit.write_text(self.unit_text)
        self.paths = (patch.object(management, 'CONFIG', self.config),
                      patch.object(management, 'UNIT', self.unit),
                      patch.object(management, 'STATE', self.state),
                      patch.object(management, 'BACKUPS', self.backups),
                      patch.object(management, 'LOCK', self.lock),
                      patch.object(management.install_player, 'verify'),
                      patch.object(management.install_player.installer, 'run'),
                      patch.object(management, '_succeeds', return_value=False))
        for item in self.paths:
            item.start()
        self.verify = management.install_player.verify

    def tearDown(self):
        for item in reversed(self.paths):
            item.stop()
        self.temporary.cleanup()

    def devices(self):
        return [{'id': 'a' * 64, 'route': 'plughw:CARD=Other,DEV=1', 'name': 'Other', 'available': True}]

    def revision(self):
        return management.inventory_revision(self.devices(), management.digest(self.unit_text.encode()))

    def test_root_service_command_is_bounded(self):
        management._run(['systemctl', 'daemon-reload'])
        management.install_player.installer.run.assert_called_once_with(
            ['systemctl', 'daemon-reload'], timeout=management.SYSTEMCTL_TIMEOUT)

    def test_refuses_customized_unit(self):
        self.unit.write_text(self.unit_text + '# local change\n')
        with self.assertRaisesRegex(ValueError, 'customized'):
            management.apply_selection(1, 'a' * 64, self.revision(), self.devices())

    def test_failed_verify_restores_config_and_unit_and_records_attempt(self):
        self.verify.side_effect = [RuntimeError('offline'), None]
        with patch.object(management.player, 'apply_files', side_effect=management.player.apply_files) as apply:
            with self.assertRaisesRegex(RuntimeError, 'previous configuration was restored'):
                management.apply_selection(4, 'a' * 64, self.revision(), self.devices())
        self.assertTrue(apply.called)
        self.assertEqual(self.unit.read_text(), self.unit_text)
        self.assertEqual(json.loads(self.config.read_text()), self.cfg)
        state = json.loads(self.state.read_text())
        self.assertEqual(state['attempted_revision'], 4)
        self.assertEqual(state['status'], 'failed')

    def test_partial_file_write_is_restored_and_restarts_active_service(self):
        changed = self.unit_text.replace('Pro,DEV=0', 'Other,DEV=1')
        next_cfg = dict(self.cfg, output='plughw:CARD=Other,DEV=1',
                        unit_digest=management.digest(changed.encode()))
        next_config = json.dumps(next_cfg, indent=2) + '\n'
        self.unit.write_text(changed)
        self.state.parent.mkdir(parents=True)
        previous = json.dumps(self.cfg)
        self.state.write_text(json.dumps({'pending': {
            'previous': {'unit': self.unit_text, 'config': previous,
                         'unit_digest': management.digest(self.unit_text.encode()),
                         'config_digest': management.digest(previous.encode())},
            'intended': {'unit': changed, 'config': next_config,
                         'unit_digest': management.digest(changed.encode()),
                         'config_digest': management.digest(next_config.encode())},
            'service': {'active': True, 'enabled': True}}}))
        self.state.chmod(0o600)
        management.recover_interrupted_apply()
        self.assertEqual(self.unit.read_text(), self.unit_text)
        self.assertEqual(json.loads(self.config.read_text()), self.cfg)
        self.assertNotIn('pending', json.loads(self.state.read_text()))
        management.install_player.installer.run.assert_any_call(
            ['systemctl', 'restart', management.SERVICE], timeout=management.SYSTEMCTL_TIMEOUT)

    def test_failed_rollback_keeps_journal_for_next_recovery(self):
        management._succeeds.return_value = True
        self.verify.side_effect = RuntimeError('offline')
        with self.assertRaisesRegex(RuntimeError, 'unverified rollback'):
            management.apply_selection(7, 'a' * 64, self.revision(), self.devices())
        state = json.loads(self.state.read_text())
        self.assertIn('pending', state)
        self.assertEqual(state['error'], 'Unverified rollback; local recovery is still required')

    def test_failed_revision_is_not_restarted(self):
        self.state.parent.mkdir(parents=True)
        self.state.write_text(json.dumps({'attempted_revision': 6, 'applied_revision': 5,
                                          'status': 'failed', 'error': 'offline'}))
        self.state.chmod(0o600)
        result = management.apply_selection(6, 'a' * 64, self.revision(), self.devices())
        self.assertEqual(result['status'], 'failed')
        self.verify.assert_not_called()


class DaemonTests(unittest.TestCase):
    def test_management_config_rejects_redirectable_or_oversized_values(self):
        with tempfile.TemporaryDirectory() as temporary:
            config = Path(temporary) / 'management.json'
            config.write_text(json.dumps({'server_url': 'https://example.test/path?x=1',
                                          'client_id': 'client', 'token': 'secret'}))
            config.chmod(0o640)
            with patch.object(management, 'MANAGEMENT_CONFIG', config):
                root_owned = os.stat_result((stat.S_IFREG | 0o640, 0, 0, 1, 0,
                                              os.getegid(), 0, 0, 0, 0))
                with patch.object(Path, 'lstat', return_value=root_owned):
                    with self.assertRaisesRegex(ValueError, 'URL'):
                        management.management_config()

    def test_report_payload_does_not_contain_hardware_routes(self):
        payload = management.report_payload({'inventory_revision': 'revision',
                                             'devices': [{'id': 'id', 'name': 'DAC', 'available': True,
                                                          'route': 'plughw:CARD=Hidden,DEV=0'}],
                                             'selected_device_id': 'id', 'selection_supported': True},
                                            {'attempted_revision': 3, 'applied_revision': 2,
                                             'status': 'failed', 'error': 'offline'})
        self.assertNotIn('route', json.dumps(payload))
        self.assertEqual(payload['attempted_revision'], 3)

if __name__ == '__main__': unittest.main()
