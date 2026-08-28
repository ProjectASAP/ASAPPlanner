---
name: asap-test-driven-development
description: Define ProjectASAP acceptance behavior and tests before implementation. Use for new behavior where test-first development or independent test design is requested or required.
---

# Test-driven development

Derive observable behavior from the requirement or approved design before
examining a proposed implementation when practical. Cover representative success,
boundary, and failure behavior at the lowest useful layer plus the applicable
end-to-end path.

Give each test a one- or two-line behavioral purpose. Confirm a new test fails
for the expected reason before implementing the behavior, then make the smallest
production change that passes it without weakening assertions.

Prefer independent test design for consequential work. State who or what designed
the tests and never label them independent when the implementation author also
defined them. Run focused and relevant regression suites and report actual
results.
