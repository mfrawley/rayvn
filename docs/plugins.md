# Maven plugins

rayvn does not run Maven plugins. It reads the configuration of the few that change what a build produces and does that work itself; every other plugin is ignored.

| Plugin | What rayvn does |
|---|---|
| `maven-compiler-plugin` | Reads `<release>`, `<source>`, `<target>` (they beat the `maven.compiler.*` properties) and calls `javac`. |
| `maven-resources-plugin` | Copies `src/main/resources` unfiltered, skipping Maven's default excludes (`.gitignore`, `.svn`, editor files). |
| `maven-jar-plugin` | Writes the jar and manifest. |
| `maven-shade-plugin` | Builds the fat jar: `ServicesResourceTransformer`, `<shadedArtifactAttached>`, `<shadedClassifierName>`. Relocations and `artifactSet` are reported as ignored. |
| `maven-surefire-plugin`, `maven-failsafe-plugin` | Read for test selection and `argLine`; the tests run through the JUnit Platform Console Launcher (`docs/testing.md`). |
| anything else | Not run. |

## `exec-maven-plugin`: not needed yet

`exec:java` and `exec:exec` run a program by hand (`mvn exec:java`). A build only needs the plugin when an `<execution>` binds it to a phase such as `generate-sources` or `package`, so that its output feeds the jar.

In the transport-modeling workspace (24 modules) no module does that. `aws-infra` declares the plugin with a `mainClass`, and `cdk.json` runs `mvn exec:java` to start the CDK app; it has no `<execution>`, so `rvn build` produces the same jar without it.

So rayvn does not run `exec`, and nothing we have validated needs it. A POM that binds it to a phase will build differently from Maven; at present rayvn does not warn about this, which is a gap. Running the program yourself is `java -cp "$(rvn classpath):target/classes" io.moia.aws.infra.Run`.

The same holds for `protoc` (`protobuf-maven-plugin`, `protoc-jar-maven-plugin`): its generated sources are needed only when the plugin is active, and the one POM that mentions it has it commented out.

## How Gradle and Bazel treat it

Gradle has no plugin phases to bind to: a task such as `JavaExec` or a code generator is wired into the task graph with declared inputs and outputs, so it runs only when its inputs change. Bazel does the same with a `genrule` or a purpose-built rule, in a sandbox. A build step with side effects is something rayvn will have to describe in the same way (declared inputs and outputs, as the test cache already does) before it runs one; running arbitrary plugin code is what the design avoids.
