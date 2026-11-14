import voice_clone
from voice_clone.hfp import doctor


def test_doctor_command_reports_instead_of_crashing_when_bluez_is_unreachable(monkeypatch, capsys):
    async def no_bluez(adapter):
        raise OSError("no system bus")

    monkeypatch.setattr(doctor, "_bluez", no_bluez)
    monkeypatch.setattr(doctor, "_kernel_lines", lambda: [])

    assert voice_clone.main(["hfp", "doctor"]) == 1
    out = capsys.readouterr().out
    assert "FAIL  cannot reach bluetoothd on the system bus: no system bus" in out
    assert "libsbc" in out
