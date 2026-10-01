# Contributor workflow

> **Proposal** (#79): agreed by a contributor, waiting for a maintainer's confirmation. Follow it for now and comment on the issue if you disagree.

How a change goes from an idea to `main`, for people and for AI agents working on their behalf. The goal is that nobody builds the same thing twice and every PR is easy to review.

## 1. Check before you start

Search the open issues **and** open pull requests for the area you want to work on, including who is assigned:

```bash
gh issue list --search "lod in:title,body" --state open
gh pr list --search "lod" --state open
```

If someone is assigned to an issue, or has an open PR in the same area, comment there before you start. A short "I'd like to help with X" saves a duplicate PR.

## 2. Claim the work

- **Find or open an issue** for what you plan to do. Use the issue templates; a feature gets a short proposal first if it is large or changes how something works.
- **Assign yourself.** If you have triage or write access, use "Assign to me". If you don't, comment "I'm taking this" and ask a maintainer to assign you; before starting large work, wait until you are assigned, because step 1's search looks at assignees.
- **Large features** get one umbrella issue, with the phases as separate issues, each assigned to whoever builds it.
- Work you have built but not sent yet can go in one issue listing those branches, so others can see it. Each item leaves the list once it has its own issue or PR.

## 3. Build

- Branch from the latest `main`, one topic per branch.
- Run what CI runs before you push (see "Run What CI Runs" in [CONTRIBUTING.md](../../CONTRIBUTING.md)).
- Keep a PR to one change a reviewer can hold in their head. Split a large feature into PRs that each work on their own.

## 4. Open the pull request

- Fill in the PR template, and write the text as [writing-prs.md](writing-prs.md) describes: the result first, the problem with an example, honest evidence.
- **Draft or ready:** open a draft when the code works but something is still undecided or unmeasured, and say what at the end of "For reviewers". Mark it ready for review once that is settled.
- **Link the issue:** `Closes #N` when the PR finishes it, `Part of #N` when it only helps.
- **Labels:** apply the labels in [labels.md](labels.md) that fit.
- **Depending on another PR:** avoid it where you can. If you can't, put `**Merge after:** #N` at the top and tell reviewers which commits to read.
- **How many at once:** keep about five PRs open at a time, so review keeps up.

## 5. Review

- CodeRabbit reviews every non-draft PR automatically. Answer each of its comments: fix it, or say why not, then resolve the thread. Unresolved threads block the merge.
- Answer human reviewers point by point, in their order: **Fixed** (with the commit), **No change** (with the reason), or **Unresolved**.
- If `main` moves while your PR is open, merge `main` into your branch (or use the "Update branch" button). Don't rebase or force-push a branch that is under review.

## 6. Merge (maintainers)

`main` accepts squash or rebase merges, requires the `tests_pass` check, requires the branch to be up to date with `main`, and requires every review thread to be resolved.

- Use **Squash and merge**. Keep the PR title as the commit title.
- Description: the PR's one-sentence summary, an empty line, then one trailer line per co-author or assistant of the PR's commits (`Co-authored-by:` or `Assisted-by:`, see [AI_POLICY.md](../AI_POLICY.md)). Drop the "Merge branch main" lines and the list of commit titles.
- After a merge, the other open PRs are behind `main` and need it merged in again before they can merge.

## AI agents

Agents follow the same steps, plus [AI_POLICY.md](../AI_POLICY.md). In particular, an agent searches issues and PRs before starting (step 1), never claims or posts without its human's OK, and says in one line that it wrote or helped with a text.
