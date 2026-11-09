"""Self-tests for rust/tools/parity/parity.py, the guard against silently dropped tests."""

from __future__ import annotations

from pathlib import Path

import pytest

import parity


@pytest.fixture
def repo(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    (tmp_path / 'tests').mkdir()
    (tmp_path / 'rust' / 'parity').mkdir(parents=True)
    (tmp_path / 'rust' / 'docs').mkdir()
    (tmp_path / 'tests' / 'test_alpha.py').write_text(
        'def test_one():\n    pass\n\nasync def test_two():\n    pass\n\ndef helper():\n    pass\n\n'
        'class TestGroup:\n    def test_three(self):\n        pass\n\n    def not_a_test(self):\n        pass\n',
        encoding='utf-8',
    )
    (tmp_path / 'tests' / 'test_beta.py').write_text('def test_four():\n    pass\n', encoding='utf-8')
    (tmp_path / 'tests' / 'helpers.py').write_text('def test_ignored():\n    pass\n', encoding='utf-8')
    monkeypatch.setattr(parity, 'REPO_ROOT', tmp_path)
    monkeypatch.setattr(parity, 'RUST_DIR', tmp_path / 'rust')
    monkeypatch.setattr(parity, 'TESTS_DIR', tmp_path / 'tests')
    monkeypatch.setattr(parity, 'SCOPE_FILE', tmp_path / 'rust' / 'parity' / 'scope.toml')
    monkeypatch.setattr(parity, 'WAIVERS_FILE', tmp_path / 'rust' / 'parity' / 'waivers.toml')
    monkeypatch.setattr(parity, 'PARITY_MD', tmp_path / 'rust' / 'docs' / 'PARITY.md')
    _scope(tmp_path, alpha='pending', beta='pending')
    (tmp_path / 'rust' / 'parity' / 'waivers.toml').write_text('', encoding='utf-8')
    return tmp_path


def _scope(root: Path, *, alpha: str, beta: str, extra: str = '') -> None:
    (root / 'rust' / 'parity' / 'scope.toml').write_text(
        f'[WP1]\nstatus = "{alpha}"\ntests = ["tests/test_alpha.py"]\n\n[WP2]\nstatus = "{beta}"\ntests = ["tests/test_beta.py"{extra}]\n',
        encoding='utf-8',
    )


def _rust(root: Path, text: str) -> None:
    path = root / 'rust' / 'crates' / 'demo' / 'src' / 'lib.rs'
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding='utf-8')


def _collect(**kwargs: Path) -> tuple[parity.Parity, list[str]]:
    report = parity.Report()
    result = parity.collect(report, pytest_listing=kwargs.get('pytest_listing'), rust_listing=kwargs.get('rust_listing'))
    return result, report.errors


def test_collects_functions_async_functions_and_class_methods_like_pytest(repo: Path) -> None:
    assert parity.python_tests() == [
        'tests/test_alpha.py::test_one',
        'tests/test_alpha.py::test_two',
        'tests/test_alpha.py::TestGroup::test_three',
        'tests/test_beta.py::test_four',
    ]


def test_claims_attach_through_stacked_comments_and_attributes(repo: Path) -> None:
    _rust(
        repo,
        '#[cfg(test)]\nmod tests {\n'
        '    // py: tests/test_alpha.py::test_one\n'
        '    // py: tests/test_alpha.py::test_two\n'
        '    #[tokio::test(flavor = "multi_thread")]\n'
        '    // the second claim covers the async variant\n'
        '    async fn both() {}\n\n'
        '    // py: tests/test_beta.py::test_four\n'
        '    #[test]\n'
        '    #[should_panic]\n'
        '    fn four() {}\n'
        '}\n',
    )
    result, errors = _collect()
    assert errors == []
    assert {test_id: [claim.name for claim in claims] for test_id, claims in result.claims.items()} == {
        'tests/test_alpha.py::test_one': ['both'],
        'tests/test_alpha.py::test_two': ['both'],
        'tests/test_beta.py::test_four': ['four'],
    }
    assert result.state('tests/test_alpha.py::TestGroup::test_three') == 'pending'


def test_a_claim_on_a_non_test_function_is_rejected(repo: Path) -> None:
    _rust(repo, '// py: tests/test_alpha.py::test_one\nfn helper() {}\n')
    result, errors = _collect()
    assert result.claims == {}
    assert any('must sit directly above a test function' in error for error in errors)


def test_a_claim_naming_a_missing_python_test_is_rejected(repo: Path) -> None:
    _rust(repo, '// py: tests/test_alpha.py::test_invented\n#[test]\nfn invented() {}\n')
    _, errors = _collect()
    assert any('names no Python test' in error for error in errors)


def test_a_claim_on_a_test_cargo_does_not_compile_is_rejected(repo: Path) -> None:
    _rust(repo, '// py: tests/test_alpha.py::test_one\n#[test]\nfn orphan() {}\n')
    listing = repo / 'cargo-tests.txt'
    listing.write_text('demo::tests::other: test\n', encoding='utf-8')
    _, errors = _collect(rust_listing=listing)
    assert any('cargo does not list it' in error for error in errors)
    listing.write_text('demo::tests::orphan: test\n', encoding='utf-8')
    _, errors = _collect(rust_listing=listing)
    assert errors == []


def test_waivers_need_a_real_test_and_a_reason(repo: Path) -> None:
    (repo / 'rust' / 'parity' / 'waivers.toml').write_text(
        '[[waiver]]\ntest = "tests/test_alpha.py::test_one"\nreason = "short"\n\n'
        '[[waiver]]\ntest = "tests/test_alpha.py::test_nope"\nreason = "The behaviour does not exist in the Rust build at all."\n',
        encoding='utf-8',
    )
    _, errors = _collect()
    assert any('needs a reason' in error for error in errors)
    assert any('is not a Python test' in error for error in errors)


def test_a_done_package_may_have_nothing_pending(repo: Path) -> None:
    _scope(repo, alpha='pending', beta='done')
    _, errors = _collect()
    assert errors == ['WP2 is done but tests/test_beta.py::test_four is neither ported nor waived']

    (repo / 'rust' / 'parity' / 'waivers.toml').write_text(
        '[[waiver]]\ntest = "tests/test_beta.py::test_four"\nreason = "Covers a code path the Rust build does not have."\n',
        encoding='utf-8',
    )
    result, errors = _collect()
    assert errors == []
    assert result.state('tests/test_beta.py::test_four') == 'waived'


def test_a_specific_pattern_moves_a_test_to_another_package(repo: Path) -> None:
    _scope(repo, alpha='pending', beta='pending', extra=', "tests/test_alpha.py::test_t*"')
    result, errors = _collect()
    assert errors == []
    assert result.assignment['tests/test_alpha.py::test_one'] == 'WP1'
    assert result.assignment['tests/test_alpha.py::test_two'] == 'WP2'
    assert result.assignment['tests/test_alpha.py::TestGroup::test_three'] == 'WP1'


def test_every_test_needs_exactly_one_owner(repo: Path) -> None:
    (repo / 'rust' / 'parity' / 'scope.toml').write_text(
        '[WP1]\ntests = ["tests/test_alpha.py"]\n\n[WP2]\ntests = ["tests/test_alpha.py"]\n', encoding='utf-8'
    )
    _, errors = _collect()
    assert any('tests/test_beta.py::test_four is not assigned' in error for error in errors)
    assert any('equally specific' in error for error in errors)


def test_pytest_cross_check_reports_both_directions(repo: Path) -> None:
    listing = repo / 'collected.txt'
    listing.write_text(
        'tests/test_alpha.py::test_one[case-a]\ntests/test_alpha.py::test_one[case-b]\ntests/test_alpha.py::test_two\n'
        'tests/test_alpha.py::TestGroup::test_three\ntests/test_gamma.py::test_dynamic\n\n5 tests collected in 0.1s\n',
        encoding='utf-8',
    )
    _, errors = _collect(pytest_listing=listing)
    assert 'pytest collects tests/test_gamma.py::test_dynamic, which the AST scan missed' in errors
    assert 'AST scan found tests/test_beta.py::test_four, which pytest does not collect' in errors


def test_parity_md_must_be_regenerated(repo: Path) -> None:
    assert parity.run(write=False, pytest_listing=None, rust_listing=None) == 1
    assert parity.run(write=True, pytest_listing=None, rust_listing=None) == 0
    assert parity.run(write=False, pytest_listing=None, rust_listing=None) == 0
    text = (repo / 'rust' / 'docs' / 'PARITY.md').read_text(encoding='utf-8')
    assert '4 Python test functions at the baseline: 0 ported, 0 waived, 4 pending.' in text


def test_tests_cannot_be_parked_in_an_invented_package(repo: Path) -> None:
    _scope(repo, alpha='pending', beta='pending', extra=']\n\n[WP99]\ntests = ["tests/test_beta.py::test_four"')
    _, errors = _collect()
    assert any('WP99 is not a work package of the plan' in error for error in errors)


def test_a_package_done_on_the_base_branch_keeps_its_status_and_tests(repo: Path) -> None:
    base = repo / 'base-scope.toml'
    base.write_text(
        '[WP1]\nstatus = "pending"\ntests = ["tests/test_alpha.py"]\n\n[WP2]\nstatus = "done"\ntests = ["tests/test_beta.py"]\n',
        encoding='utf-8',
    )
    (repo / 'rust' / 'parity' / 'waivers.toml').write_text(
        '[[waiver]]\ntest = "tests/test_beta.py::test_four"\nreason = "Covers a code path the Rust build does not have."\n',
        encoding='utf-8',
    )
    _scope(repo, alpha='pending', beta='done')
    report = parity.Report()
    parity.collect(report, pytest_listing=None, rust_listing=None, base_scope=base)
    assert report.errors == []

    _scope(repo, alpha='pending', beta='pending')
    report = parity.Report()
    parity.collect(report, pytest_listing=None, rust_listing=None, base_scope=base)
    assert any('WP2 was done on the base branch and cannot be reopened' in error for error in report.errors)

    (repo / 'rust' / 'parity' / 'scope.toml').write_text(
        '[WP1]\nstatus = "pending"\ntests = ["tests/test_alpha.py", "tests/test_beta.py::test_four"]\n\n'
        '[WP2]\nstatus = "done"\ntests = ["tests/test_beta.py"]\n',
        encoding='utf-8',
    )
    report = parity.Report()
    parity.collect(report, pytest_listing=None, rust_listing=None, base_scope=base)
    assert any('belonged to WP2, done on the base branch; it cannot move to WP1' in error for error in report.errors)


def test_the_final_audit_requires_nothing_pending_anywhere(repo: Path) -> None:
    (repo / 'rust' / 'parity' / 'scope.toml').write_text(
        '[WP1]\nstatus = "pending"\ntests = ["tests/test_alpha.py"]\n\n[WP18]\nstatus = "done"\ntests = ["tests/test_beta.py"]\n',
        encoding='utf-8',
    )
    (repo / 'rust' / 'parity' / 'waivers.toml').write_text(
        '[[waiver]]\ntest = "tests/test_beta.py::test_four"\nreason = "Covers a code path the Rust build does not have."\n',
        encoding='utf-8',
    )
    _, errors = _collect()
    assert sum('the parity audit) is done but' in error for error in errors) == 3
