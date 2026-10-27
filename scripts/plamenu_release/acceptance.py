"""Narrow 0.7 acceptance of reviewed historical evidence, never a full pass."""

import json
import shutil
import subprocess
import tomllib
from pathlib import Path

from .core import ReleaseError, digest, fingerprint, reusable, write_json

POLICY = "release/0.7-validation-exceptions.json"
# Only release plumbing, documentation prose and the standalone diagnostic may
# differ from the reviewed repair revision. Installation scripts remain protected.
RELEASE_ONLY = {
    "CHANGELOG.md",
    POLICY,
    "accessibility/probe-media-runtime.mjs",
    "accessibility/inventory.json",
    "scripts/release.py",
    "scripts/ci/quick.sh",
    "scripts/plamenu_release/acceptance.py",
    "scripts/tests/test_release.py",
    "scripts/tests/test_release_acceptance.py",
}


def source_fingerprint(source, revision):
    tree = subprocess.check_output(
        ["git", "ls-tree", "-rz", revision], cwd=source
    ).decode()
    entries = []
    for entry in tree.split("\0"):
        if not entry:
            continue
        metadata, name = entry.split("\t", 1)
        if name == "accessibility/inventory.json":
            inventory = json.loads(
                subprocess.check_output(
                    ["git", "show", f"{revision}:{name}"], cwd=source
                )
            )
            # The diagnostic is newly inventoried, but every existing case and
            # source entry must remain identical to the reviewed inventory.
            inventory["files"].pop("accessibility/probe-media-runtime.mjs", None)
            entries.append([name, metadata.split()[:2], fingerprint(inventory)])
            continue
        if name in RELEASE_ONLY or (name.startswith("docs/") and name.endswith(".md")):
            continue
        entries.append([name, metadata])
    return fingerprint(entries)


def validate(source, evidence_root, version, revision):
    if version != "0.7.0":
        raise ReleaseError("Reviewed validation exceptions apply only to 0.7.0")
    policy = json.loads((Path(source) / POLICY).read_text())
    if (
        policy.get("schema") != 1
        or policy.get("version") != version
        or policy.get("full_pass") is not False
        or policy.get("wcag_conformance") is not False
        or not policy.get("evidence")
    ):
        raise ReleaseError("Invalid 0.7 validation exception policy")
    if source_fingerprint(source, revision) != policy["validated_source_fingerprint"]:
        raise ReleaseError(
            "Application, fixture or test inputs changed since the reviewed audit; "
            "0.7 historical exceptions cannot qualify these changes"
        )
    root = Path(evidence_root).resolve()
    for relative, expected in policy["evidence"].items():
        path = root / relative
        if Path(relative).is_absolute() or ".." in Path(relative).parts:
            raise ReleaseError("Invalid exception evidence path")
        if not path.is_file() or digest(path) != expected:
            raise ReleaseError("Missing or changed reviewed evidence: " + relative)
    return {
        "status": "accepted-with-exceptions",
        "full_pass": False,
        "wcag_conformance": False,
        "source_revision": revision,
        "full_audit_revision": policy["full_audit_revision"],
        "repair_revision": policy["repair_revision"],
        "validated_source_revision": policy.get(
            "validated_source_revision", policy["repair_revision"]
        ),
        "supplemental_validation": policy.get("supplemental_validation", []),
        "validation_method": "historical full audit plus focused repairs and any recorded supplements; no final-revision full rerun",
        "accepted_gaps": policy["accepted_gaps"],
        "policy_sha256": digest(Path(source) / POLICY),
        "evidence_sha256": policy["evidence"],
    }


def record(runner, folder, evidence_root, version, revision):
    accepted = validate(runner.source, evidence_root, version, revision)
    # Retain the original non-passing records locally, byte-for-byte. Only the
    # concise acceptance receipt and their hashes go into public release.json.
    for relative in accepted["evidence_sha256"]:
        destination = folder / "evidence" / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(Path(evidence_root) / relative, destination)
        if digest(destination) != accepted["evidence_sha256"][relative]:
            raise ReleaseError("Reviewed evidence changed while copying: " + relative)
    write_json(folder / "acceptance.json", accepted)


def accessibility_status():
    return {
        "status": "accepted-exception",
        "full_pass": False,
        "wcag_conformance": False,
        "reason": "0.7 reviewed browser exclusions and unperformed human verification",
    }


def application_reuse(source, previous, revision):
    """Reuse only a complete 0.7 application run across the reviewed profile change."""
    previous = Path(previous).resolve()
    identity = json.loads((previous / "identity.json").read_text())
    key = fingerprint({"identity": identity, "stage": "application"})
    if (
        identity.get("version") != "0.7.0"
        or not reusable(previous / "application", key)
        or (previous / "application/reuse.json").exists()
    ):
        raise ReleaseError(
            "Application reuse requires an intact, directly executed 0.7 stage"
        )
    old = identity["source"]
    if subprocess.run(
        ["git", "merge-base", "--is-ancestor", old, revision], cwd=source, check=False
    ).returncode:
        raise ReleaseError("Application reuse source must be an ancestor")

    def manifest(ref):
        return tomllib.loads(
            subprocess.check_output(
                ["git", "show", f"{ref}:Cargo.toml"], cwd=source, text=True
            )
        )

    before, after = manifest(old), manifest(revision)
    for value, expected in ((before, ("fat", 1)), (after, ("thin", 16))):
        deploy = value.get("profile", {}).get("deploy", {})
        actual = (deploy.pop("lto", None), deploy.pop("codegen-units", None))
        if actual != expected:
            raise ReleaseError(
                "Application reuse only permits fat/1 to thin/16 deployment settings"
            )
    if before != after:
        raise ReleaseError("Application Cargo inputs changed")
    changed = subprocess.check_output(
        ["git", "diff", "--name-only", "--no-renames", old, revision],
        cwd=source,
        text=True,
    ).splitlines()
    permitted = RELEASE_ONLY - {"accessibility/inventory.json"}
    permitted |= {"Cargo.toml", ".forgejo/workflows/release-worker.yml"}
    if any(
        name not in permitted
        and not (name.startswith("docs/") and name.endswith(".md"))
        for name in changed
    ):
        raise ReleaseError(
            "Application, dependency or test inputs changed; cannot reuse application checks"
        )
    return {
        "source_revision": old,
        "stage_sha256": digest(previous / "application/stage.json"),
        "identity_sha256": digest(previous / "identity.json"),
        "execution": "reused; not rerun on the release revision",
        "scope": "application tests and fresh SQLx metadata; deploy artifacts must be rebuilt and tested",
        "full_pass": False,
    }


def record_application_reuse(runner, folder, previous, revision, expected):
    if application_reuse(runner.source, previous, revision) != expected:
        raise ReleaseError("Application reuse evidence changed after preflight")
    shutil.copytree(Path(previous) / "application", folder / "previous")
    shutil.copy2(Path(previous) / "identity.json", folder / "previous-identity.json")
    # Check the copied bytes as well, without modifying the original record.
    identity = json.loads((folder / "previous-identity.json").read_text())
    if not reusable(
        folder / "previous", fingerprint({"identity": identity, "stage": "application"})
    ):
        raise ReleaseError("Application reuse evidence changed while copying")
    write_json(folder / "reuse.json", expected)


DISCLOSURE = (
    "\n\n### Validation scope\n\n"
    "This experimental 0.7 release accepts explicitly recorded validation gaps. "
    "The historical full audit and focused repair checks are not a clean full pass "
    "on this release revision (`full_pass=false`). Human accessibility verification "
    "is incomplete; no WCAG conformance claim is made. "
    "See `release.json` for the accepted gaps, source revisions and evidence hashes."
)
