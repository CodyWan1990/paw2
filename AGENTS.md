# Paw2

Paw2 is going to be a Wayland compositor written in rust with smithay and will eventually have a quckshell layered on top 

## Working rules

- **IMPORTANT** make the code as human readable as possible
- Prefer the smallest change that preserves existing behavior.
- Search for existing components, helpers, services, and patterns before adding
  code.
- Never duplicate shared behavior:
  - Reuse an existing helper or component when one fits.
  - When behavior is needed in multiple places, extract it at the narrowest
    appropriate layer.
  - Keep one-use logic local unless it represents a project-wide invariant.
- Do not introduce speculative abstractions, dependencies, or configuration.
- Preserve unrelated working-tree changes.
- Do not commit or push unless explicitly requested.
- Run the smallest relevant checks. Broader smoke testing is only needed when
  the changed behavior cannot be checked statically or with a focused test.
- When completing a session always give user install/change instructions, do not assume the user knows what you've changed or how to install  

## Product boundary

- `paw2` must be secure, lightweight and focused on gaming 
- treat wayland native gaming as the first class option but make sure xwayland support is good for legacy games that dont run well on wayland 

## Branches

Substantial or potentially breaking work must happen on `feature/<slug>`, not
`master`. This includes:

- packaging or installation changes
- major compositor behavior
- major UI or settings redesigns

Continue on an existing appropriate feature branch when one already exists.
Small fixes, refactors, tests, and documentation do not require a new branch.

## Documentation and personal configuration

- `README.md` is the user-facing overview: what paw is, the feature set,
  installing the release, roadmap, and licensing. Keep technical reference out
  of it. Instead keep valid technical references and project layout in a `REFERENCE.md` and have it git ignored, this will be your scrathpad for relevant information, project structure, and past mistakes not to repeat
