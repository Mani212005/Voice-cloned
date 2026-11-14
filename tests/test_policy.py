import pytest

from voice_clone.hfp.policy import Action, PolicyError, PolicyStore, parse_policy


def test_default_is_ai_for_everyone(tmp_path):
    store = PolicyStore(tmp_path / "missing.txt")
    assert store.decide("+919876543210") == Action.AI
    assert store.decide(None) == Action.AI
    assert PolicyStore(None).decide("1") == Action.AI


def test_rules_exact_suffix_wildcard_and_withheld():
    policy = parse_policy(
        """
        # comment line
        *                 ring
        withheld          deny   # nobody without caller ID
        +91 98765-43210   ai
        12345             deny
        """.splitlines()
    )
    assert policy.decide("+919876543210") == Action.AI
    assert policy.decide("+91 (987) 654 3210") == Action.AI
    assert policy.decide("+4412345") == Action.DENY  # suffix match for numbers without +
    assert policy.decide("555") == Action.RING
    assert policy.decide(None) == Action.DENY
    assert policy.decide("") == Action.DENY


def test_longest_suffix_wins():
    policy = parse_policy(["3210 ring", "6543210 deny"])
    assert policy.decide("+919876543210") == Action.DENY
    assert policy.decide("+910000003210") == Action.RING


def test_withheld_defaults_to_wildcard():
    assert parse_policy(["* ring"]).decide(None) == Action.RING


@pytest.mark.parametrize(
    "line",
    ["+91123", "+91123 maybe", "not-a-number ai"],
)
def test_bad_lines_are_rejected(line):
    with pytest.raises(PolicyError):
        parse_policy([line])


def test_store_rereads_file(tmp_path):
    path = tmp_path / "policy.txt"
    path.write_text("* ai\n")
    store = PolicyStore(path)
    assert store.decide("1") == Action.AI
    path.write_text("* ring\n")
    assert store.decide("1") == Action.RING
