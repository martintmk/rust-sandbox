# Licensed under the MIT License.

# >>> anvil-managed: anvil-imports
import 'justfiles/anvil/mod.just'
# <<< anvil-managed: anvil-imports

# >>> anvil-managed: anvil-runner
anvil_runner := env_var_or_default("ANVIL_RUNNER", "native")
# <<< anvil-managed: anvil-runner
