#!/usr/bin/env python3
"""Inspect or reapply repository settings without changing visibility or CI secrets."""

import argparse
import json
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--token-file", type=Path, required=True)
    parser.add_argument(
        "--apply",
        action="store_true",
        help="apply ci/codefloe-settings.json; default is read-only",
    )
    args = parser.parse_args()
    token = args.token_file.read_text().strip()
    if not token:
        raise SystemExit("Empty token file")
    opener = urllib.request.build_opener(NoRedirect())
    repo = "/repos/plamenu/plamenu"

    def api(path, method="GET", body=None):
        request = urllib.request.Request(
            "https://codefloe.com/api/v1" + path,
            method=method,
            headers={
                "Authorization": "token " + token,
                "Content-Type": "application/json",
            },
            data=None if body is None else json.dumps(body).encode(),
        )
        try:
            with opener.open(request, timeout=30) as response:
                data = response.read()
                return json.loads(data) if data else None
        except urllib.error.HTTPError as error:
            raise SystemExit(f"Codefloe {method} {path}: HTTP {error.code}") from None

    current = api(repo)
    if args.apply:
        config = json.loads(
            (
                Path(__file__).resolve().parents[1] / "ci/codefloe-settings.json"
            ).read_text()
        )
        if "private" in config["repository"]:
            raise SystemExit(
                "Remove 'private' from repository settings; change visibility separately"
            )
        teams = api("/orgs/plamenu/teams?limit=100")
        team = next((t for t in teams if t["name"] == "maintainers"), None)
        if team is None:
            team = api(
                "/orgs/plamenu/teams",
                "POST",
                {
                    "name": "maintainers",
                    "permission": "write",
                    "description": "Plamenu code review, CI and release maintenance",
                    "includes_all_repositories": False,
                    "can_create_org_repo": False,
                    "units": [
                        "repo.code",
                        "repo.issues",
                        "repo.pulls",
                        "repo.releases",
                        "repo.actions",
                    ],
                },
            )
        api(f"/teams/{team['id']}/repos/plamenu/plamenu", "PUT")
        for username in config["maintainers"]:
            api(
                f"/teams/{team['id']}/members/{urllib.parse.quote(username, safe='')}",
                "PUT",
            )
        api(repo, "PATCH", config["repository"])
        labels = {label["name"] for label in api(repo + "/labels?limit=100")}
        for label in config.get("labels", []):
            if label["name"] not in labels:
                api(repo + "/labels", "POST", label)
        rules = api(repo + "/branch_protections")
        if any(r["rule_name"] == "main" for r in rules):
            api(repo + "/branch_protections/main", "PATCH", config["main_protection"])
        else:
            api(repo + "/branch_protections", "POST", config["main_protection"])
        tags = api(repo + "/tag_protections")
        tag = next((t for t in tags if t["name_pattern"] == "v*"), None)
        api(
            repo + (f"/tag_protections/{tag['id']}" if tag else "/tag_protections"),
            "PATCH" if tag else "POST",
            config["release_tags"],
        )
        current = api(repo)
    print(
        json.dumps(
            {
                "repository": {
                    k: current[k]
                    for k in [
                        "full_name",
                        "private",
                        "default_branch",
                        "has_actions",
                        "has_releases",
                    ]
                },
                "branch_protections": api(repo + "/branch_protections"),
                "tag_protections": api(repo + "/tag_protections"),
                "teams": [
                    {k: t[k] for k in ["id", "name", "permission"]}
                    for t in api("/orgs/plamenu/teams?limit=100")
                ],
            },
            indent=2,
        )
    )


if __name__ == "__main__":
    main()
