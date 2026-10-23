"""USB wake safety contracts; every sysfs attribute here is a temporary file.

Run in the development cluster with the complete installer suite. These tests
deliberately exercise non-PCI topology as well as desktop root hubs.
"""
import importlib.util
from pathlib import Path
import sys
import tempfile
import json
import socket
import threading
import unittest
from unittest.mock import Mock
from io import BytesIO


class PolicyTests(unittest.TestCase):
    def test_private_config_rejects_world_readable_files_and_symlinks(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'config'
            path.write_text('secret')
            path.chmod(0o644)
            with self.assertRaises(ValueError):
                wake.private_read(path)
            alias = path.with_name('alias')
            alias.symlink_to(path)
            with self.assertRaises(OSError):
                wake.private_read(alias)

    def test_saved_binding_rejects_paths_outside_devices_before_io(self):
        for path in ('/etc/passwd', '/sys/devices/../kernel/disable', 'relative/disable'):
            with self.assertRaises(ValueError):
                wake.load_binding({'device': '/sys/devices/dac', 'disable_path': path,
                                   'identity': ['1234', '5678', '00', 'serial']})

    def test_invalid_urls_and_redirects_are_rejected(self):
        for url in ('file:///etc/passwd', 'http://user:secret@host', 'http://host?x=1'):
            with self.assertRaises(ValueError):
                wake.PolicyClient(url, 'token')
        with self.assertRaises(ValueError):
            wake.NoRedirect().redirect_request(None, None, 302, '', {}, 'http://other')

    def test_policy_validates_types_bounds_and_size(self):
        for value in ({'enabled': True, 'idle_seconds': 15},
                      {'enabled': 'false', 'idle_seconds': 15},
                      {'enabled': True, 'idle_seconds': True},
                      {'enabled': True, 'idle_seconds': 0}, 'x' * 65537):
            client = wake.PolicyClient('http://host', 'token')
            response = BytesIO(json.dumps(value).encode())
            client.opener = Mock()
            client.opener.open.return_value = response
            if isinstance(value, dict) and type(value['idle_seconds']) is int and value['idle_seconds'] == 15 and value['enabled'] is True:
                self.assertEqual(client.observe(), value)
            else:
                with self.assertRaises(ValueError):
                    client.observe()


spec = importlib.util.spec_from_file_location(
    'musicata_usb_autowake',
    Path(__file__).resolve().parents[2] / 'packaging/usb_autowake.py',
)
wake = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = wake
spec.loader.exec_module(wake)


class UsbBindingTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / 'sys'
        self.root.mkdir()

    def device(self, controller='platform/soc/example-usb', bus=3, number=2):
        hub = self.root / 'devices' / controller / f'usb{bus}'
        device = hub / f'{bus}-{number}'
        device.mkdir(parents=True)
        for name, value in [('idVendor', '1234'), ('idProduct', '5678'),
                            ('serial', 'fixture-dac'), ('bDeviceClass', '00')]:
            (device / name).write_text(value + '\n')
        port = hub / f'{bus}-0:1.0' / f'usb{bus}-port{number}'
        port.mkdir(parents=True)
        (port / 'disable').write_text('0\n')
        (port / 'device').symlink_to(device)
        return device, port

    def test_discovers_non_pci_port_without_host_or_dac_constants(self):
        device, port = self.device()
        binding = wake.UsbPort.discover(device, sysfs_root=self.root)
        self.assertEqual(binding.disable_path, port / 'disable')
        binding.set_enabled(False)
        self.assertEqual((port / 'disable').read_text().strip(), '1')

    def test_discovery_does_not_toggle_port(self):
        device, port = self.device()
        wake.UsbPort.discover(device, sysfs_root=self.root)
        self.assertEqual((port / 'disable').read_text(), '0\n')

    def test_serial_less_device_cannot_establish_unattended_identity(self):
        device, port = self.device()
        (device / 'serial').unlink()
        with self.assertRaises(ValueError):
            wake.UsbPort.discover(device, sysfs_root=self.root)
        self.assertEqual((port / 'disable').read_text(), '0\n')

    def test_selected_device_does_not_touch_neighbor(self):
        selected, port = self.device()
        _, neighbor = self.device(number=4)
        binding = wake.UsbPort.discover(selected, sysfs_root=self.root)
        binding.set_enabled(False)
        self.assertEqual((port / 'disable').read_text().strip(), '1')
        self.assertEqual((neighbor / 'disable').read_text(), '0\n')

    def test_refuses_hub_as_dac(self):
        device, port = self.device()
        (device / 'bDeviceClass').write_text('09\n')
        with self.assertRaises(ValueError):
            wake.UsbPort.discover(device, sysfs_root=self.root)
        self.assertEqual((port / 'disable').read_text(), '0\n')

    def test_refuses_ambiguous_port_mapping(self):
        device, port = self.device()
        _, other = self.device(number=4)
        (other / 'device').unlink()
        (other / 'device').symlink_to(device)
        with self.assertRaises(ValueError):
            wake.UsbPort.discover(device, sysfs_root=self.root)
        self.assertEqual((port / 'disable').read_text(), '0\n')

    def test_changed_device_identity_cannot_be_disabled(self):
        device, port = self.device()
        binding = wake.UsbPort.discover(device, sysfs_root=self.root)
        (device / 'serial').write_text('replacement-device\n')
        with self.assertRaises(ValueError):
            binding.set_enabled(False)
        self.assertEqual((port / 'disable').read_text(), '0\n')

    def test_reenables_bound_port_after_device_files_disappear(self):
        device, port = self.device()
        binding = wake.UsbPort.discover(device, sysfs_root=self.root)
        binding.set_enabled(False)
        (port / 'device').unlink()
        for path in device.iterdir():
            path.unlink()
        device.rmdir()
        binding.set_enabled(False)  # Already disabled: no identity read or write needed.
        binding.set_enabled(True)
        self.assertEqual((port / 'disable').read_text().strip(), '0')

    def test_replaced_control_symlink_cannot_write_outside_binding(self):
        device, port = self.device()
        binding = wake.UsbPort.discover(device, sysfs_root=self.root)
        unrelated = self.root.parent / 'unrelated'
        unrelated.write_text('preserve')
        (port / 'disable').unlink()
        (port / 'disable').symlink_to(unrelated)
        with self.assertRaises(ValueError):
            binding.set_enabled(False)
        self.assertEqual(unrelated.read_text(), 'preserve')


class IdleDecisionTests(unittest.TestCase):
    def test_policy_disabled_restores_bound_port_and_unknown_policy_does_nothing(self):
        from unittest.mock import Mock
        port = Mock()
        controller = wake.Controller(port)
        controller.step(None, ('music', 'idle'), 0)
        port.set_enabled.assert_not_called()
        controller.step({'enabled': False, 'idle_seconds': 15}, None, 1)
        port.set_enabled.assert_called_once_with(True)

    def test_policy_change_and_failed_observation_reset_idle(self):
        from unittest.mock import Mock
        port = Mock()
        controller = wake.Controller(port)
        policy = {'enabled': True, 'idle_seconds': 15}
        controller.step(policy, ('music', 'idle'), 0)
        controller.step(policy, None, 14)
        controller.step(policy, ('music', 'idle'), 20)
        port.set_enabled.assert_not_called()
        controller.step(policy, ('music', 'idle'), 35)
        port.set_enabled.assert_called_once_with(False)
        controller.step(policy, ('music', 'playing'), 36)
        port.set_enabled.assert_called_with(True)

    def test_idle_requires_continuous_interval(self):
        gate = wake.IdleGate(idle_seconds=15)
        self.assertIsNone(gate.observe({'snapcast': 'idle'}, now=0))
        self.assertIsNone(gate.observe({'snapcast': 'idle'}, now=14.9))
        self.assertIs(gate.observe({'snapcast': 'idle'}, now=15), False)

    def test_mpd_playing_keeps_dac_enabled_when_snapcast_is_idle(self):
        gate = wake.IdleGate(idle_seconds=15)
        gate.observe({'snapcast': 'idle', 'mpd': 'idle'}, now=0)
        self.assertIs(gate.observe({'snapcast': 'idle', 'mpd': 'playing'}, now=20), True)
        self.assertIsNone(gate.observe({'snapcast': 'idle', 'mpd': 'idle'}, now=21))

    def test_unknown_state_resets_idle_without_port_action(self):
        gate = wake.IdleGate(idle_seconds=15)
        gate.observe({'snapcast': 'idle'}, now=0)
        self.assertIsNone(gate.observe({'snapcast': None}, now=14))
        self.assertIsNone(gate.observe({'snapcast': 'idle'}, now=100))
        self.assertIs(gate.observe({'snapcast': 'idle'}, now=115), False)

    def test_one_idle_backend_does_not_override_an_unknown_backend(self):
        gate = wake.IdleGate(idle_seconds=15)
        for now in (0, 15, 100):
            self.assertIsNone(gate.observe({'snapcast': 'idle', 'mpd': None}, now=now))

    def test_empty_source_list_cannot_disable_port(self):
        gate = wake.IdleGate(idle_seconds=15)
        for now in (0, 100):
            self.assertIsNone(gate.observe({}, now=now))

    def test_source_change_requires_fresh_idle_interval(self):
        gate = wake.IdleGate(idle_seconds=15)
        gate.observe({'first-stream': 'idle'}, now=0)
        self.assertIsNone(gate.observe({'second-stream': 'idle'}, now=100))


class ProtocolTests(unittest.TestCase):
    def serve(self, handler):
        listener = socket.socket()
        listener.bind(('127.0.0.1', 0))
        listener.listen()
        listener.settimeout(3)
        self.addCleanup(listener.close)
        errors = []

        def run():
            try:
                with listener.accept()[0] as connection:
                    connection.settimeout(3)
                    with connection.makefile('rwb') as stream:
                        handler(stream)
            except Exception as error:
                errors.append(error)

        thread = threading.Thread(target=run, daemon=True)
        thread.start()

        def finish():
            thread.join(4)
            self.assertFalse(thread.is_alive())
            self.assertEqual(errors, [])

        self.addCleanup(finish)
        return listener.getsockname()[1]

    def snap_reply(self, stream, status='playing', stream_id='music'):
        request = json.loads(stream.readline())
        server = {'groups': [{'clients': [{'id': 'chosen'}], 'stream_id': stream_id}],
                  'streams': [{'id': stream_id, 'status': status}]}
        stream.write(b'{"jsonrpc":"2.0","method":"Stream.OnUpdate"}\n')
        stream.write((json.dumps({'id': request['id'], 'result': {'server': server}}) + '\n').encode())
        stream.flush()

    def test_snapcast_reuses_connection_and_follows_assignment(self):
        def handler(stream):
            self.snap_reply(stream)
            self.snap_reply(stream, 'idle', 'changed')
        client = wake.SnapcastStatus('127.0.0.1', self.serve(handler), 'chosen')
        self.addCleanup(client.close)
        self.assertEqual(client.observe(), ('music', 'playing'))
        self.assertEqual(client.observe(), ('changed', 'idle'))

    def test_snapcast_missing_client_is_unknown(self):
        client = wake.SnapcastStatus('127.0.0.1', self.serve(self.snap_reply), 'absent')
        self.addCleanup(client.close)
        with self.assertRaises(ValueError):
            client.observe()
        self.assertIsNone(client.connection)

    def test_snapcast_unknown_stream_status_is_rejected(self):
        client = wake.SnapcastStatus('127.0.0.1', self.serve(
            lambda stream: self.snap_reply(stream, 'disabled')), 'chosen')
        self.addCleanup(client.close)
        with self.assertRaises(ValueError):
            client.observe()

    def test_mpd_persistent_status_maps_play_pause_stop(self):
        def handler(stream):
            stream.write(b'OK MPD 0.23.0\n')
            stream.flush()
            for state in ('play', 'pause', 'stop'):
                self.assertEqual(stream.readline(), b'status\n')
                stream.write(('volume: 50\nstate: ' + state + '\nOK\n').encode())
                stream.flush()
        client = wake.MpdStatus('127.0.0.1', self.serve(handler))
        self.addCleanup(client.close)
        self.assertEqual([client.observe() for _ in range(3)], ['playing', 'idle', 'idle'])

    def test_mpd_error_does_not_become_idle(self):
        def handler(stream):
            stream.write(b'OK MPD 0.23.0\n')
            stream.flush()
            stream.readline()
            stream.write(b'ACK [4@0] {status} permission denied\n')
            stream.flush()
        client = wake.MpdStatus('127.0.0.1', self.serve(handler))
        self.addCleanup(client.close)
        with self.assertRaises(ValueError):
            client.observe()
        self.assertIsNone(client.connection)

    def test_mpd_missing_state_is_unknown(self):
        def handler(stream):
            stream.write(b'OK MPD 0.23.0\n')
            stream.flush()
            stream.readline()
            stream.write(b'volume: 50\nOK\n')
            stream.flush()
        client = wake.MpdStatus('127.0.0.1', self.serve(handler))
        self.addCleanup(client.close)
        with self.assertRaises(ValueError):
            client.observe()

    def test_response_byte_limit_closes_connection(self):
        def handler(stream):
            stream.readline()
            stream.write(b'x' * 128)
            stream.flush()
        client = wake.SnapcastStatus('127.0.0.1', self.serve(handler), 'chosen', byte_limit=64)
        self.addCleanup(client.close)
        with self.assertRaises(ValueError):
            client.observe()
        self.assertIsNone(client.connection)

    def test_timeout_closes_connection_and_next_observation_reconnects(self):
        def handler(stream):
            stream.readline()
            # Waiting for EOF avoids timing-sensitive sleeps in the fixture.
            self.assertEqual(stream.read(), b'')
        client = wake.SnapcastStatus('127.0.0.1', self.serve(handler), 'chosen', timeout=0.05)
        self.addCleanup(client.close)
        with self.assertRaises((TimeoutError, socket.timeout)):
            client.observe()
        self.assertIsNone(client.connection)
        client.port = self.serve(self.snap_reply)
        client.timeout = 3
        self.assertEqual(client.observe(), ('music', 'playing'))


if __name__ == '__main__':
    unittest.main()
