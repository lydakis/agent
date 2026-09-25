"""Harbor installed-agent adapter: runs one Agent bot per benchmark task.

Harbor (https://github.com/laude-institute/harbor) runs Terminal-Bench, SWE-bench,
GAIA, tau2-bench and other suites against a harness installed in each task's
container. This adapter uploads a static Linux `agent` binary, runs the task
instruction as one blocking `agent run` in the task's working directory, and
reports the bot's provider-reported tokens back to Harbor. It needs Harbor's
Python environment, not bench/requirements.txt; see docs/HARBOR.md.

    harbor run -d terminal-bench/terminal-bench-2-1 -a bench.harbor_agent:Agent \
        -m anthropic/claude-opus-5-5
"""

import asyncio
import contextlib
import json
import os
import shlex
import sqlite3
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
CHATGPT_URL = 'https://chatgpt.com/backend-api/codex'
# SQLite stays on the container's own disk: the log directory is a host mount.
REMOTE_STORE = '/tmp/agent-harbor'
BOT = 'task'
# Shutdown waits up to 30 s for the daemon to exit; the rest is a few execs.
FINISH_TIMEOUT = 60



def ran_on(turn_provider: str, attempt: dict[str, Any]) -> str:
    """The model an attempt in a usage event's `models` split ran on: its own
    provider when the event names one (a summarizer's), else the turn's."""
    provider = attempt['provider'] + '/' if 'provider' in attempt else turn_provider
    return provider + attempt['model']


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
        ErrorPattern(r'"error":"(?:provider_connection_[a-z0-9_]+|provider_stream_failed'
                     r'|truncated_sse_frame|provider_admission_timeout)"', NetworkConnectionError),
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
        # A chatgpt/ model signs in with the ChatGPT login Codex saved, unless
        # a chatgpt spec names another endpoint, which never gets the login.
        if ((self.model_name or '').startswith('chatgpt/')
                and not any(p.partition('=')[0] == 'chatgpt' for p in self._providers)):
            self._providers.append('chatgpt')
        self._chatgpt = any(self._signs_in(spec) for spec in self._providers)
        codex_home = os.environ.get('CODEX_HOME') or Path.home() / '.codex'
        self._codex_auth = Path(codex_auth or Path(codex_home) / 'auth.json')

    @staticmethod
    def _signs_in(spec: str) -> bool:
        """Whether the daemon reads Codex's login for this spec: the chatgpt
        name at its own endpoint with no key variable, as ProviderSpec::parse."""
        name, _, rest = spec.partition('=')
        fields = [field for field in rest.split(',') if field]
        url = fields[1] if len(fields) > 1 else CHATGPT_URL
        return name == 'chatgpt' and len(fields) < 3 and url == CHATGPT_URL

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
            # Only what a request carries: the refresh and ID tokens stay on
            # the host, where the model's tools cannot read them. The daemon
            # redacts the access token from tool output.
            await self.exec_as_root(environment, command=f'mkdir -p -m 755 {REMOTE_CODEX_HOME}')
            await self._upload_config_text(
                environment, content=json.dumps({'tokens': self._chatgpt_login()}),
                remote_path=f'{REMOTE_CODEX_HOME}/auth.json', filename='auth.json')

    def _chatgpt_login(self) -> dict[str, str]:
        tokens = json.loads(self._codex_auth.read_text()).get('tokens') or {}
        login = {key: tokens.get(key) for key in ('access_token', 'account_id')}
        if not all(isinstance(value, str) and value for value in login.values()):
            raise ValueError(f'{self._codex_auth} has no ChatGPT login; run `codex login`')
        return login

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
        # The turn's exit status decides the trial, not tee's.
        return (f'mkdir -p {REMOTE_STORE} {logs}; '
                f'{run} < /dev/null | tee {logs}/agent.jsonl; exit ${{PIPESTATUS[0]}}')

    @staticmethod
    def _finish_command() -> str:
        """Stop the daemon and save its store, the trial's accounting record.

        Bots the task delegated to may still be running, and after Harbor's
        timeout so is the task bot: shutdown cancels their turns and returns
        once the daemon has committed them and exited, so the copy is final.
        """
        logs = EnvironmentPaths.agent_dir.as_posix()
        store = f'{REMOTE_STORE}/state.sqlite'
        return (
            # No socket: the daemon never started, so nothing ran.
            f'[ -S {store}.sock ] || exit 0; '
            f'agent stats --no-spawn > {logs}/stats.json; '
            f'agent shutdown && cp {store} {logs}/ && '
            # A clean exit checkpoints the WAL away; a crash may leave it.
            f'for f in {store}-wal {store}-shm; do [ ! -e "$f" ] || cp "$f" {logs}/; done'
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
        try:
            await self.exec_as_agent(environment, command=self._command(instruction),
                                     env=self._env())
        finally:
            # Also after Harbor's timeout cancels this: the command it gave up
            # on, and the daemon that runs its turn in its own process group,
            # would otherwise keep calling the model and tools in the container.
            try:
                async with asyncio.timeout(FINISH_TIMEOUT):
                    await self.exec_as_agent(environment, command=self._finish_command(),
                                             env=self._env())
            except Exception as error:  # bookkeeping never replaces the trial's outcome
                self.logger.warning(f'lydakis-agent bookkeeping failed: {error}')

    @override
    def populate_context_post_run(self, context: AgentContext) -> None:
        # Every bot's turns, so tokens a delegated bot spent on another model
        # are priced at that model's rates.
        turns = self._store_turns() or []
        models: dict[str, ModelUsage] = {}
        for turn in turns:
            usage = models.setdefault(turn['model'], ModelUsage())
            usage.n_input_tokens += turn['input_tokens']
            usage.n_cache_tokens += turn['cached_input_tokens']
            usage.n_output_tokens += turn['output_tokens']
        events = self._usage_events()
        if not models and (streamed := self._streamed_usage()):
            model = self.model_name or 'unknown'
            models[model] = ModelUsage(
                n_input_tokens=sum(data['input_tokens'] for data in streamed),
                n_cache_tokens=sum(data['cached_input_tokens'] for data in streamed),
                n_output_tokens=sum(data['output_tokens'] for data in streamed),
            )
            events = [(model, data) for data in streamed]
        # A call Anthropic's server-side fallback, or a summarizer, ran on
        # another model is in its turn's totals; move each attempt to the
        # model that ran it.
        moved = set()
        for turn_model, attempts in ((m, d['models']) for m, d in events if 'models' in d):
            provider = turn_model.split('/', 1)[0] + '/' if '/' in turn_model else ''
            moved.add(turn_model)
            for attempt in attempts:
                for model, sign in ((turn_model, -1), (ran_on(provider, attempt), 1)):
                    usage = models.setdefault(model, ModelUsage())
                    usage.n_input_tokens += sign * attempt['input_tokens']
                    usage.n_cache_tokens += sign * attempt['cached_input_tokens']
                    usage.n_output_tokens += sign * attempt['output_tokens']
        # Cache writes are inside input tokens; they are priced above input.
        writes: dict[str, int] = {}
        for model, data in events:
            provider = model.split('/', 1)[0] + '/' if '/' in model else ''
            for attempt in data.get('models') or [data]:
                key = ran_on(provider, attempt) if 'model' in attempt else model
                writes[key] = writes.get(key, 0) + attempt.get('cache_write_tokens', 0)
        # A turn sticky routing served entirely elsewhere leaves nothing to price.
        for model in moved:
            usage = models[model]
            if not (usage.n_input_tokens or usage.n_cache_tokens or usage.n_output_tokens):
                del models[model]
        if not models:
            return
        for model, usage in models.items():
            usage.cost_usd = self._cost(model, usage, writes.get(model, 0))
        costs = [usage.cost_usd for usage in models.values()]
        context.n_input_tokens = sum(u.n_input_tokens for u in models.values())
        context.n_cache_tokens = sum(u.n_cache_tokens for u in models.values())
        context.n_output_tokens = sum(u.n_output_tokens for u in models.values())
        # A model the price table lacks leaves the trial's cost unknown, not low.
        context.cost_usd = None if None in costs else sum(costs)
        context.model_usage = models
        if turns:
            context.metadata = {key: sum(t[key] for t in turns)
                                for key in ('model_rounds', 'retries', 'paced_ms')}
            context.metadata['status'] = [t['status'] for t in turns if t['bot'] == BOT]
            context.metadata['bots'] = len({t['bot'] for t in turns})

    def _store_turns(self) -> list[dict[str, Any]] | None:
        """Every turn in the store copied after the daemon exited, or None when
        there is no copy: the daemon never started or its shutdown failed.

        Read from the final store rather than asked of the running daemon, so
        a delegated bot's last call, or a bot created at the end, is counted.
        The columns and model fallback are the daemon's own `turns` listing's.
        """
        path = self.logs_dir / 'state.sqlite'
        if not path.is_file():
            return None
        try:
            with contextlib.closing(sqlite3.connect(path)) as db:
                db.row_factory = sqlite3.Row
                return [dict(row) for row in db.execute(
                    "SELECT t.bot,COALESCE(t.model,b.provider||'/'||b.model) AS model,t.status,"
                    't.input_tokens,t.cached_input_tokens,t.output_tokens,'
                    't.model_rounds,t.retries,t.paced_ms '
                    'FROM turns t JOIN bots b ON b.name=t.bot ORDER BY t.id')]
        except sqlite3.Error:
            return None

    def _usage_events(self) -> list[tuple[str, dict[str, Any]]]:
        """Every call's usage event in the store, with the model its turn is
        counted under."""
        path = self.logs_dir / 'state.sqlite'
        if not path.is_file():
            return []
        try:
            with contextlib.closing(sqlite3.connect(path)) as db:
                return [(model, json.loads(data)) for model, data in db.execute(
                    "SELECT COALESCE(t.model,b.provider||'/'||b.model),e.data "
                    'FROM events e JOIN turns t ON t.id=e.turn JOIN bots b ON b.name=t.bot '
                    "WHERE e.kind='usage'")]
        except sqlite3.Error:
            return []

    def _streamed_usage(self) -> list[dict[str, Any]] | None:
        """The task bot's per-call usage events, streamed as they happened.

        Used when there is no store copy; bots it delegated to are not in
        this stream.
        """
        path = self.logs_dir / 'agent.jsonl'
        if not path.is_file():
            return None
        usage = []
        for line in path.read_text().splitlines():
            try:
                event = json.loads(line)
            except json.JSONDecodeError:  # diagnostics, or a line cut by the kill
                continue
            if event.get('event') == 'usage':
                usage.append(event['data'])
        return usage or None

    @staticmethod
    def _cost(model: str, usage: ModelUsage, writes: int = 0) -> float | None:
        """Aggregate-token cost from LiteLLM's table, as Harbor's own adapters do.

        `writes` of the uncached input tokens went to the prompt cache and are
        billed at the model's cache-write rate (the 5-minute rate the runtime
        requests), or as input when the table has none.
        """
        try:
            import litellm
        except ImportError:
            return None
        key = next((k for k in (model, model.split('/', 1)[-1]) if litellm.model_cost.get(k)), None)
        if key is None:
            return None
        rates = litellm.model_cost[key]
        uncached = usage.n_input_tokens - usage.n_cache_tokens
        cached_rate = rates.get('cache_read_input_token_cost') or rates['input_cost_per_token']
        write_rate = rates.get('cache_creation_input_token_cost') or rates['input_cost_per_token']
        return ((uncached - writes) * rates['input_cost_per_token'] + writes * write_rate
                + usage.n_cache_tokens * cached_rate + usage.n_output_tokens * rates['output_cost_per_token'])
