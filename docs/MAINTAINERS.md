# Maintain repository access

Use this guide to inspect or restore the repository's settings. Workflow
operation is covered by [CI/CD](CI-CD.md), and publication by the
[release procedure](RELEASING.md).

## Inspect or apply settings

Run from the source checkout. Replace the token-file path with your local
Codefloe administration credential:

```sh
python3 scripts/codefloe-settings.py --token-file /path/to/codefloe-token
```

The command prints the repository, branch protections, tag protections, and
organization teams. Save this output before changing settings. To apply
[ci/codefloe-settings.json](https://codefloe.com/plamenu/plamenu/src/branch/main/ci/codefloe-settings.json):

```sh
python3 scripts/codefloe-settings.py --token-file /path/to/codefloe-token --apply
```

The script works with private and public repositories and preserves their
current visibility. Visibility is deliberately absent from the settings file;
adding a `private` field makes the command stop before applying changes.
Repository publication and package-owner visibility are separate launch steps.

It adds configured maintainers and missing labels; it does not remove team
members. Review team membership separately when granting or withdrawing access.
Keep the administration token outside CI.

## Protections

- `main` accepts pull-request merges after both quick and Clippy checks pass.
  Direct and force pushes are disabled, including for administrators.
- Rejected reviews and outdated branches block merging. New commits dismiss
  stale approvals. The independent-approval count is currently zero.
- The `maintainers` team has write access and is named in `CODEOWNERS`.
  Only that team may create `v*` tags.

Settings can still be changed by repository administrators. Check the applied
rules and access after changing team membership or recreating the repository.

## Commit and release credentials

Sign authored commits and include the DCO sign-off with `git commit -S -s`.
Verify the signature locally and on Codefloe after pushing. DCO trailers are
checked by contribution CI; they do not verify the cryptographic signature.

[Release signing](RELEASING.md#publishing-setup) lists the
publication secrets, trusted public keys, and key recovery procedure.
