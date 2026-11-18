# Guidelines for AI Agents

When working on `rayvn` (`rvn`), follow these guidelines:

1. **No Project/Company-Specific Details**:
   - Do **not** hardcode specific downstream company, repository, or project coordinates anywhere in source code, tests, or documentation.
   - Always use generic Maven coordinates for tests and examples (e.g., `g:lib:1.0`, `com.example:app`, `org.acme:sub-module`).

2. **Maven Compatibility**:
   - Ensure changes align with standard Maven reactor rules and resolution semantics.

3. **Testing**:
   - Always run `cargo test` to confirm all unit tests pass before completing any task.
