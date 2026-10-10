---
name: plan
description: Keep your plan where the person sees it, for any task of three or more steps, and update it as each step starts and ends
---

# Your plan

The app shows your plan beside your name: how many steps are done, the step
you are on, and the whole list at the top of your chat. Keep it current, so
the person can see where you are without asking.

Write the whole plan in one call, one argument a step, each starting with
`[x] ` (done), `[>] ` (doing now) or `[ ] ` (to do):

```
sh "$HOME/.agents/skills/plan/plan" '[x] Read the provider layout' '[>] Write the signer' '[ ] Add signing tests' '[ ] Wire it into the provider table'
```

Each call replaces the plan, so send every step each time, in order. Call it
when you start the work, when a step starts or ends, and when the plan
changes. Name a step for its outcome, in a few words. Keep it to 30 steps of
at most 200 bytes each; group small ones. With no arguments it prints the
plan as it stands.

When the work is done and you have said so, or you turn to something the plan
does not describe, clear it, so the person does not read an old plan as your
current one:

```
sh "$HOME/.agents/skills/plan/plan" --clear
```

The plan is for the person, not a record: what you found and decided still
goes in your replies.
