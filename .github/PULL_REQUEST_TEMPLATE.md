<!--
The title must look like `type(scope): summary`, for example `fix(mp4): reject a trun that claims
more samples than it holds`. Releases are versioned from it.
-->

## What and why

<!-- What changes, and the problem it solves. Link the issue if there is one. -->

## How it was tested

<!-- The test you added, and for behaviour a player sees, what you played it in. -->

## Checklist

- [ ] `make ci` passes
- [ ] There is a test that fails without this change
- [ ] Anything read from a file or a mapper is bounded before it is allocated
- [ ] The handbook is updated if operators, mapper authors, or players see the change
- [ ] Every commit is signed off (`git commit -s`)
- [ ] I did not copy or adapt code from a project with an incompatible licence (see CONTRIBUTING.md)
