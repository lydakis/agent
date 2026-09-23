"""Harbor installed-agent adapter: runs one Agent bot per benchmark task.

Harbor (https://github.com/laude-institute/harbor) runs Terminal-Bench, SWE-bench,
GAIA, tau2-bench and other suites against a harness installed in each task's
container. This adapter uploads a static Linux `agent` binary, runs the task
instruction as one blocking `agent run` in the task's working directory, and
reports the bot's provider-reported tokens back to Harbor. It needs Harbor's
Python environment, not bench/requirements.txt; see docs/HARBOR.md.

    harbor run -d terminal-bench@2.0 --agent-import-path bench.harbor_agent:Agent \
        -m anthropic/claude-opus-5-5
"""

import json
import os
import shlex
from pathlib import Path
from typing import Any, ClassVar, override

from harbor.agents.installed.base import (
    AgentAuthenticationError,
    ApiInternalServerError,
    ApiOverloadedError,
    ApiRateLimitError,
    ApiResponseStalledError,
    ApiUsageLimitError,
    BaseInstalledAgent,
    ErrorPattern,
    NetworkConnectionError,
    PackageSpec,
    with_prompt_template,
)
from harbor.agents.model_connection import ModelConnectionSpec
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext, ModelUsage
from harbor.models.trial.paths import EnvironmentPaths

REPOSITORY = Path(__file__).resolve().parents[1]
DEFAULT_BINARY = REPOSITORY / '.local/target/x86_64-unknown-linux-musl/release/agent'
REMOTE_BINARY = '/installed-agent/agent'
REMOTE_CODEX_HOME = '/installed-agent/codex'
# SQLite stays on the container's own disk: the log directory is a host mount.
REMOTE_STORE = '/tmp/agent-harbor'
BOT = 'task'


class Agent(BaseInstalledAgent):
    """The lydakis/agent daemon, one bot per trial.

    Keyword arguments (Harbor `--ak key=value`):
      binary       host path to a static Linux build (default: the musl
                   release build under .local/target, or AGENT_HARBOR_BINARY)
      provider     extra `--provider` spec, or a list of them, for gateways
                   such as Bedrock; the spec's KEY_ENV is forwarded
      reasoning    `--reasoning` level
      codex_auth   for chatgpt/MODEL: Codex's ChatGPT login to copy into the
                   container (default: $CODEX_HOME/auth.json, else ~/.codex/auth.json)
      max_output_tokens, stall_timeout, context_bytes, compact_at: daemon limits
    """

    # Resolve the provider from the model prefix and forward its key env
    # (ANTHROPIC_API_KEY for anthropic/..., OPENAI_API_KEY for openai/...).
    MODEL_CONNECTION = ModelConnectionSpec(passthrough=True)
    ERROR_PATTERNS: ClassVar[list[ErrorPattern]] = [
        ErrorPattern(r'"error":"provider_(?:rate_limited|http_429|paced)"', ApiRateLimitError),
        ErrorPattern(r'"error":"provider_quota_exhausted"', ApiUsageLimitError),
        ErrorPattern(r'"error":"provider_http_(?:500|502|504)"', ApiInternalServerError),
        ErrorPattern(r'"error":"provider_(?:http_503|http_529|unavailable)"', ApiOverloadedError),
        ErrorPattern(r'"error":"provider_stream_stalled"', ApiResponseStalledError),
        ErrorPattern(r'"error":"provider_connection_[a-z0-9_]+"', NetworkConnectionError),
        ErrorPattern(r'"error":"provider_(?:http_401|http_403|key_unavailable)"', AgentAuthenticationError),
    ]
    # The HTTP client reads the platform's root certificates at daemon start,
    # even for plain-HTTP endpoints, and minimal images ship none.
    SYSTEM_PACKAGES: ClassVar[dict[str, PackageSpec]] = {
        **BaseInstalledAgent.SYSTEM_PACKAGES,
        'ca_bundle': PackageSpec(
            commands=(),
            packages={m: ('ca-certificates',) for m in ('apt-get', 'dnf', 'yum', 'apk')},
            always_install=True,
        ),
    }
    CA_BUNDLES = ('/etc/ssl/certs/ca-certificates.crt', '/etc/pki/tls/certs/ca-bundle.crt',
                  '/etc/ssl/cert.pem')
    _LIMITS = {
        'max_output_tokens': '--max-output-tokens',
        'stall_timeout': '--stall-timeout',
        'context_bytes': '--context-bytes',
        'compact_at': '--compact-at',
    }

    def __init__(self, *args: Any, binary: str | None = None,
                 provider: str | list[str] | None = None, reasoning: str | None = None,
                 codex_auth: str | None = None, **kwargs: Any) -> None:
        limits = {key: kwargs.pop(key) for key in list(kwargs) if key in self._LIMITS}
        super().__init__(*args, **kwargs)
        self._binary = Path(binary or os.environ.get('AGENT_HARBOR_BINARY') or DEFAULT_BINARY)
        self._providers = [provider] if isinstance(provider, str) else list(provider or [])
        self._reasoning = reasoning
        self._limits = limits
        # A chatgpt/ model signs in with the ChatGPT login Codex saved.
        self._chatgpt = (self.model_name or '').startswith('chatgpt/')
        if self._chatgpt and not any(p.partition('=')[0] == 'chatgpt' for p in self._providers):
            self._providers.append('chatgpt')
        codex_home = os.environ.get('CODEX_HOME') or Path.home() / '.codex'
        self._codex_auth = Path(codex_auth or Path(codex_home) / 'auth.json')

    @staticmethod
    @override
    def name() -> str:
        return 'lydakis-agent'

    @override
    def get_version_command(self) -> str | None:
        return f'{REMOTE_BINARY} --version'

    @override
    def parse_version(self, stdout: str) -> str:
        return stdout.strip().split()[-1]

    @override
    async def install(self, environment: BaseEnvironment) -> None:
        if not self._binary.is_file():
            raise FileNotFoundError(
                f'{self._binary} is missing; build it with '
                'cargo build --release --locked --target x86_64-unknown-linux-musl')
        if self._chatgpt and not self._codex_auth.is_file():
            raise FileNotFoundError(
                f'{self._codex_auth} is missing; sign in with `codex login` first')
        # Harbor's exec and the run command below need bash; minimal images (Alpine) lack it.
        needed = ('bash',)
        has_roots = ' || '.join(f'[ -s {path} ]' for path in self.CA_BUNDLES)
        if (await environment.exec(command=has_roots, user='root')).return_code != 0:
            needed += ('ca_bundle',)
        await self.ensure_system_dependencies(environment, needed)
        await environment.upload_file(self._binary, REMOTE_BINARY)
        # On PATH so a bot's shell can delegate with `agent run` like it does locally.
        await self.exec_as_root(
            environment,
            command=f'chmod 755 {REMOTE_BINARY} && ln -sf {REMOTE_BINARY} /usr/local/bin/agent',
        )
        if self._chatgpt:
            remote = f'{REMOTE_CODEX_HOME}/auth.json'
            await self.exec_as_root(environment, command=f'mkdir -p -m 755 {REMOTE_CODEX_HOME}')
            await self._upload_agent_owned_file(environment, self._codex_auth, remote)
            await self.exec_as_root(environment, command=f'chmod 600 {remote}')

    def _command(self, instruction: str) -> str:
        if not self.model_name:
            raise ValueError('Model name is required, as PROVIDER/MODEL')
        flags = ['--new', '--bot', BOT, '--agents', '--model', self.model_name]
        for spec in self._providers:
            flags += ['--provider', spec]
        if self._reasoning:
            flags += ['--reasoning', self._reasoning]
        for key, value in self._limits.items():
            flags += [self._LIMITS[key], str(value)]
        logs = EnvironmentPaths.agent_dir.as_posix()
        run = shlex.join(['agent', 'run', *flags, '--', instruction])
        # The turn's exit status decides the trial; bookkeeping after it must
        # not mask it, and must still run when the turn fails.
        return (
            f'mkdir -p {REMOTE_STORE} {logs}; '
            f'{run} < /dev/null | tee {logs}/agent.jsonl; status=${{PIPESTATUS[0]}}; '
            f'agent turns --bot {BOT} > {logs}/turns.json; '
            f'agent stats > {logs}/stats.json; '
            'agent shutdown; '
            f'cp {REMOTE_STORE}/state.sqlite{{,-wal,-shm}} {logs}/ 2>/dev/null; '
            'exit $status'
        )

    def _env(self) -> dict[str, str]:
        env = {**self.resolve_env_vars(), **self.model_connection.env,
               'AGENT_STORE': f'{REMOTE_STORE}/state.sqlite'}
        if self._chatgpt:
            env['CODEX_HOME'] = REMOTE_CODEX_HOME
        for spec in self._providers:
            fields = spec.partition('=')[2].split(',')
            if len(fields) >= 3 and (value := self._get_env(fields[2])):
                env[fields[2]] = value
        return env

    @override
    @with_prompt_template
    async def run(self, instruction: str, environment: BaseEnvironment,
                  context: AgentContext) -> None:
        await self.exec_as_agent(environment, command=self._command(instruction), env=self._env())

    @override
    def populate_context_post_run(self, context: AgentContext) -> None:
        # Daemon-wide totals include any bots the task bot delegated to.
        stats = self._read_json('stats.json')
        tokens = stats['tokens'] if stats else self._streamed_usage()
        if tokens is None:
            return
        usage = ModelUsage(
            n_input_tokens=tokens['input_tokens'],
            n_cache_tokens=tokens['cached_input_tokens'],
            n_output_tokens=tokens['output_tokens'],
        )
        usage.cost_usd = self._cost(usage)
        context.n_input_tokens = usage.n_input_tokens
        context.n_cache_tokens = usage.n_cache_tokens
        context.n_output_tokens = usage.n_output_tokens
        context.cost_usd = usage.cost_usd
        context.model_usage = {self.model_name or 'unknown': usage}
        turns = self._read_json('turns.json')
        if turns:
            context.metadata = {key: sum(t[key] for t in turns)
                                for key in ('model_rounds', 'retries', 'paced_ms')}
            context.metadata['status'] = [t['status'] for t in turns]

    def _read_json(self, name: str) -> Any:
        """A bookkeeping file, or None when the daemon never started or the
        trial hit its agent timeout, which kills the command before it runs."""
        path = self.logs_dir / name
        if path.is_file() and (text := path.read_text().strip()):
            return json.loads(text)
        return None

    def _streamed_usage(self) -> dict[str, int] | None:
        """The task bot's per-round usage events, streamed as they happened.

        Used after a timeout; bots it delegated to are not in this stream.
        """
        path = self.logs_dir / 'agent.jsonl'
        if not path.is_file():
            return None
        keys = ('input_tokens', 'cached_input_tokens', 'output_tokens')
        totals = dict.fromkeys(keys, 0)
        seen = False
        for line in path.read_text().splitlines():
            try:
                event = json.loads(line)
            except json.JSONDecodeError:  # diagnostics, or a line cut by the kill
                continue
            if event.get('event') == 'usage':
                seen = True
                for key in keys:
                    totals[key] += event['data'][key]
        return totals if seen else None

    def _cost(self, usage: ModelUsage) -> float | None:
        """Aggregate-token cost from LiteLLM's table, as Harbor's own adapters do.

        Anthropic cache writes are billed above the base input rate, but the
        turn record folds them into input tokens, so Anthropic cost is a floor.
        """
        try:
            import litellm
        except ImportError:
            return None
        model = self.model_name or ''
        key = next((k for k in (model, model.split('/', 1)[-1]) if litellm.model_cost.get(k)), None)
        if key is None:
            return None
        rates = litellm.model_cost[key]
        uncached = usage.n_input_tokens - usage.n_cache_tokens
        cached_rate = rates.get('cache_read_input_token_cost') or rates['input_cost_per_token']
        return (uncached * rates['input_cost_per_token'] + usage.n_cache_tokens * cached_rate
                + usage.n_output_tokens * rates['output_cost_per_token'])
