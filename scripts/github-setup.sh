#!/usr/bin/env bash
# One-time repository setup for maintainers: description, topics (search keywords), default branch,
# security features and branch protection. Needs `gh` logged in as a repository admin.
#
#   scripts/github-setup.sh                 # configure, keep the repository private
#   scripts/github-setup.sh --make-public   # also publish it (cannot be fully undone: forks and caches persist)
set -euo pipefail
REPO="${REPO:-megalabs-dev/rustle}"
api() { gh api -H "Accept: application/vnd.github+json" "$@"; }

echo "== description, homepage, features"
api -X PATCH "repos/$REPO" \
  -f default_branch=main \
  -f description="Ultra-light self-hosted PaaS for small VPSs: deploy stateless Next.js/Node apps from GHCR images behind Caddy, with the platform under 8 MB of RAM. Rust + WebAssembly." \
  -f homepage="https://github.com/$REPO#readme" \
  -F has_wiki=false -F has_projects=false -F delete_branch_on_merge=true \
  -F allow_squash_merge=true -F allow_merge_commit=false -F allow_rebase_merge=true >/dev/null

echo "== topics"
api -X PUT "repos/$REPO/topics" \
  -f 'names[]=paas' -f 'names[]=self-hosted' -f 'names[]=heroku-alternative' -f 'names[]=vercel-alternative' \
  -f 'names[]=nextjs' -f 'names[]=deployment' -f 'names[]=devops' -f 'names[]=docker' -f 'names[]=caddy' \
  -f 'names[]=rust' -f 'names[]=webassembly' -f 'names[]=leptos' -f 'names[]=sqlite' -f 'names[]=vps' \
  -f 'names[]=ghcr' -f 'names[]=github-actions' -f 'names[]=zero-downtime' -f 'names[]=low-memory' \
  -f 'names[]=ci-cd' -f 'names[]=open-source' >/dev/null

echo "== security features"
api -X PUT "repos/$REPO/vulnerability-alerts" >/dev/null || true
api -X PUT "repos/$REPO/automated-security-fixes" >/dev/null || true
api -X PUT "repos/$REPO/private-vulnerability-reporting" >/dev/null || true

echo "== branch protection for main (after CI has run once, add the job names under required_status_checks)"
api -X PUT "repos/$REPO/branches/main/protection" --input - >/dev/null <<'JSON'
{
  "required_status_checks": null,
  "enforce_admins": false,
  "required_pull_request_reviews": {"required_approving_review_count": 1, "dismiss_stale_reviews": true},
  "restrictions": null,
  "required_linear_history": true,
  "allow_force_pushes": false,
  "allow_deletions": false
}
JSON

if [ "${1:-}" = "--make-public" ]; then
  echo "== making the repository public"
  api -X PATCH "repos/$REPO" -F private=false >/dev/null
fi
echo "done"
