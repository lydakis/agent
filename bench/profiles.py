"""Declared test boundaries, not inferred feature parity or allocation attribution."""


def profile(engine):
    common = {'scenario': 'responses_text_v1', 'durability': 'ephemeral',
              'history': 'all_prior_user_and_assistant_messages', 'tools_exercised': [],
              'provider': 'loopback_http1_responses_sse', 'retries': 0,
              'consumer': 'fast_normalized_text_events', 'isolation': 'caller_host_permissions'}
    footprints = {
        'rust': ['native_daemon', 'stdio_jsonl_protocol', 'sqlite_full_store', 'one_scheduler',
                 'bounded_text_output', 'echo_tool_registered_not_called'],
        'pi': ['node_runtime', 'agent_core_and_pi_ai', 'no_coding_agent_session_manager',
               'steering_and_tool_loop_available', 'no_tools_registered'],
        'codex': ['node_rpc_adapter', 'native_app_server', 'native_session_services',
                  'ephemeral_threads', 'native_schema_and_context_overhead',
                  'disabled_optional_features_not_proven_unallocated'],
        'fixture': ['python_binary_transport_calibration_client'],
        'fx': ['node_runtime', 'native_libfx_addon', 'thread_per_agent',
               'acp_bridge_and_host_fetch', 'in_memory_conversations', 'no_tools_registered'],
        'claude-code': ['node_stream_json_adapter', 'native_cli_process_per_agent',
                        'bare_mode', 'no_session_persistence', 'no_tools_enabled',
                        'native_system_prompt_prefix_and_context_blocks',
                        'connection_preconnect_request_per_process',
                        'git_probe_subprocesses_per_process',
                        'sampled_guards_scaled_per_agent_process',
                        'disabled_optional_features_not_proven_unallocated'],
    }
    if engine not in footprints:
        raise ValueError('unknown benchmark profile')
    if engine == 'fixture':
        common = {'scenario': 'binary_transport_calibration_v1', 'durability': 'none'}
    if engine == 'rust':
        common = {**common, 'durability': 'sqlite_full', 'consumer': 'observer_driven_stdio_protocol'}
    if engine == 'fx':
        common = {**common, 'provider': 'loopback_http1_gateway_sse',
                  'retries': 'one_pre_output_transport_retry_available_but_rejected_by_fixture'}
    if engine == 'claude-code':
        common = {**common, 'provider': 'loopback_http1_anthropic_messages_sse'}
    return {'version': 1, 'contract': common, 'footprint': footprints[engine],
            'not_exercised': ['durable_resume', 'historical_fork', 'tools', 'compaction',
                              'slow_consumer', 'cancellation', 'live_provider_auth', 'tls']}


def differences(base, candidate):
    left = base.get('target_metadata', {}).get('comparison_profile')
    right = candidate.get('target_metadata', {}).get('comparison_profile')
    gaps = []
    if not left or not right:
        return ['missing feature profile; historical/custom results cannot establish parity']
    for key in ('version', 'contract', 'footprint', 'not_exercised'):
        if key not in left or key not in right or left[key] != right[key]:
            gaps.append(f'{key} differs or is unknown: baseline={left.get(key)!r}; candidate={right.get(key)!r}')
    if base.get('target_metadata', {}).get('engine') != candidate.get('target_metadata', {}).get('engine'):
        gaps.append('different engine implementations; residual feature costs are not isolated')
    return gaps
