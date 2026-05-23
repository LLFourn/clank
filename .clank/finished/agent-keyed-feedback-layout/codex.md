APPROVE

This is a narrow doc-comment cleanup and it does what it says: the remaining production comments now point at `.clank/agents/<author>/feedback/<target>/<ref>.md` instead of the retired `.clank/feedback/<plan>/<sha>/<author>.md` shape.

I checked the diff and the intentionally old-layout references left in tests are negative fixtures, so they are fine.
