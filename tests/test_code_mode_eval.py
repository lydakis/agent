"""The code-mode evaluation's shop must answer MCP and hold one answer per
task, and its Agent arm must reach the shop through mcpx from a bot's shell,
isolated from the person's own configuration, before any paid run."""
import http.server
import io
import json
import os
import re
import shutil
import tempfile
import threading
import unittest
from pathlib import Path
from types import SimpleNamespace

from bench import code_mode_eval, code_mode_shop
from bench.code_mode_shop import answer_of, call, expected, respond, serve

BINARY = Path('.local/target/release/agent')


class Shop(unittest.TestCase):
    def test_serves_initialize_list_and_call_over_stdio(self):
        lines = [{'jsonrpc': '2.0', 'id': 1, 'method': 'initialize', 'params': {'protocolVersion': '2025-06-18'}},
                 {'jsonrpc': '2.0', 'method': 'notifications/initialized'},
                 {'jsonrpc': '2.0', 'id': 2, 'method': 'tools/list'},
                 {'jsonrpc': '2.0', 'id': 3, 'method': 'tools/call',
                  'params': {'name': 'get_product', 'arguments': {'sku': 'P17'}}},
                 {'jsonrpc': '2.0', 'id': 4, 'method': 'tools/call',
                  'params': {'name': 'get_product', 'arguments': {'sku': 'nope'}}}]
        out = io.StringIO()
        log = Path(tempfile.mkdtemp(prefix='code-mode-shop-')) / 'calls.log'
        self.addCleanup(shutil.rmtree, log.parent, True)
        serve(io.StringIO(''.join(json.dumps(m) + '\n' for m in lines)), out, log)
        self.assertEqual(log.read_text(), 'get_product\nget_product\n')  # every call is counted, failed ones too
        replies = [json.loads(line) for line in out.getvalue().splitlines()]
        self.assertEqual([r['id'] for r in replies], [1, 2, 3, 4])  # no reply to a notification
        self.assertEqual(replies[0]['result']['protocolVersion'], '2025-06-18')
        self.assertIn('search_logs', [t['name'] for t in replies[1]['result']['tools']])
        product = json.loads(replies[2]['result']['content'][0]['text'])
        self.assertEqual(f"{product['category']} {product['price_cents']}", expected()['small'])
        self.assertTrue(replies[3]['result']['isError'])
        self.assertEqual(respond({'jsonrpc': '2.0', 'id': 9, 'method': 'nope'})['error']['code'], -32601)

    def test_data_and_answers_are_fixed_by_the_seed(self):
        self.assertEqual(code_mode_shop.build()['orders'], code_mode_shop.SHOP['orders'])
        self.assertEqual(set(expected()), set(code_mode_shop.TASKS))

    def test_the_tasks_are_the_shapes_they_claim(self):
        eu = call('list_customers', {'region': 'EU', 'page_size': 50})
        self.assertGreater(eu['pages'] * 50, 50)  # fan-out: more EU customers than one page
        # The log is bigger than a tool preview and the tool cannot filter it.
        self.assertGreater(len(call('search_logs', {'service': 'billing', 'date': '2025-06-03'})), 64 * 1024)
        self.assertEqual(call('search_logs', {'service': 'billing', 'date': '2025-06-03', 'level': 'ERROR'}),
                         call('search_logs', {'service': 'billing', 'date': '2025-06-03'}))

    def test_answers_are_read_from_the_last_answer_line(self):
        self.assertEqual(answer_of('ANSWER: C0001 5\nthen\n**ANSWER: C0115, 175203**'), 'C0115 175203')
        self.assertIsNone(answer_of('no answer'))
        self.assertTrue(code_mode_shop.correct('chain', f"ANSWER: {expected()['chain'].lower()}"))

    def test_guidance_is_mcpx_skill_text_for_mcpx_arms_and_each_hint_only_in_its_arms(self):
        skill = '---\nname: mcpx\n---\n\n# mcpx\nbody\n'
        guide = code_mode_eval.guidance
        self.assertIsNone(guide('codex-native', skill))
        self.assertEqual(guide('codex-direct', skill), code_mode_eval.DIRECT_HINT + '\n')
        self.assertEqual(guide('agent-mcpx', skill), '# mcpx\nbody\n')
        self.assertEqual(guide('agent-direct', skill), f'# mcpx\nbody\n\n{code_mode_eval.DIRECT_HINT}\n')
        self.assertEqual(guide('codex-code', skill), f'# mcpx\nbody\n\n{code_mode_eval.CODE_HINT}\n')


class Model(http.server.BaseHTTPRequestHandler):
    """Looks product P17 up once, natively when the shop is a tool and through
    mcpx from the shell otherwise, then answers from what came back. Speaks
    to Agent and to Codex, which name their shell tools differently."""
    protocol_version = 'HTTP/1.1'

    def log_message(self, *_):
        pass

    def do_GET(self):  # Codex's model catalog
        self.send(b'{"models":[]}', 'application/json')

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        self.server.requests.append(request)
        offered = request.get('tools', []) + [t for item in request['input'] if item.get('type') == 'additional_tools'
                                              for t in item['tools']]
        tools = {t.get('name') for t in offered} | {f"{t['name']}.{x['name']}" for t in offered for x in t.get('tools', [])}
        code_mode = 'functions.exec' in tools
        codex = code_mode or 'exec_command' in tools
        through_mcpx = 'mcpx - MCP tools as Unix commands' in json.dumps(request['input'])
        last = request['input'][-1]
        if not last.get('type', '').endswith('_output') and code_mode:
            # Codex's code mode: one script that makes the call.
            call = ('tools.exec_command({cmd: "mcpx shop get_product --sku=P17"})' if through_mcpx
                    else 'tools.mcp__shop__get_product({sku: "P17"})')
            item = {'type': 'custom_tool_call', 'name': 'exec', 'namespace': 'functions', 'id': 'fc1', 'call_id': 'c1',
                    'input': f'text(JSON.stringify(await {call}));'}
        elif not last.get('type', '').endswith('_output'):
            if 'mcp__shop' in tools:
                item = {'type': 'function_call', 'name': 'get_product', 'namespace': 'mcp__shop',
                        'arguments': json.dumps({'sku': 'P17'})}
            else:
                command = 'mcpx shop get_product --sku=P17'
                item = {'type': 'function_call', 'name': 'exec_command' if codex else 'shell',
                        'arguments': json.dumps({'cmd' if codex else 'command': command})}
            item.update(id='fc1', call_id='c1')
        else:
            seen = json.dumps(last['output'])
            while '\\"' in seen:  # however many JSON layers wrap the tool's output
                seen = seen.replace('\\\\', '\\').replace('\\"', '"')
            product = json.loads(re.search(r'\{[^{}]*"sku"[^{}]*\}', seen)[0])
            text = f"ANSWER: {product['category']} {product['price_cents']}"
            item = {'type': 'message', 'role': 'assistant', 'id': 'm1',
                    'content': [{'type': 'output_text', 'text': text}]}
        usage = {'input_tokens': 10, 'output_tokens': 2, 'total_tokens': 12,
                 'input_tokens_details': {'cached_tokens': 4}, 'output_tokens_details': {'reasoning_tokens': 0}}
        events = [{'type': 'response.created', 'response': {'id': 'r'}}]
        if codex:
            events += [{'type': 'response.output_item.done', 'item': item},
                       {'type': 'response.completed', 'response': {'id': 'r', 'status': 'completed', 'output': [],
                                                                  'usage': usage}}]
        else:
            if item['type'] == 'message':
                events.append({'type': 'response.output_text.delta', 'delta': item['content'][0]['text']})
            events.append({'type': 'response.completed', 'response': {'status': 'completed', 'output': [item],
                                                                      'usage': usage}})
        self.send(''.join(f"event: {e['type']}\ndata: {json.dumps(e)}\n\n" for e in events).encode(),
                  'text/event-stream')

    def send(self, body, kind):
        self.send_response(200)
        self.send_header('Content-Type', kind)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class Arms(unittest.TestCase):
    def setUp(self):
        self.server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Model)
        self.server.requests = []
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        self.out = Path(tempfile.mkdtemp(prefix='code-mode-test-'))
        self.addCleanup(shutil.rmtree, self.out, True)
        url = f'http://127.0.0.1:{self.server.server_port}/v1'
        # A throwaway login, so no run can touch the person's own.
        login = self.out / 'login'
        login.mkdir()
        (login / 'auth.json').write_text('{}')
        env = {'FAKE_KEY': 'x', 'CODEX_HOME': str(login)}
        saved = {k: os.environ.get(k) for k in env}
        os.environ.update(env)
        self.addCleanup(lambda: [os.environ.pop(k) if v is None else os.environ.__setitem__(k, v)
                                 for k, v in saved.items()])
        self.args = SimpleNamespace(
            model='m', reasoning='low', timeout=120, agent=str(BINARY.resolve()), mcpx='mcpx', codex='codex',
            provider=f'fake=responses,{url},FAKE_KEY',
            codex_config=['model_provider = "fake"', '[model_providers.fake]', 'name = "fake"',
                          f'base_url = "{url}"', 'wire_api = "responses"', 'env_key = "FAKE_KEY"'])
        self.skill = code_mode_eval.mcpx_skill('mcpx')

    def instructions(self):
        return json.dumps(self.server.requests[0])

    @unittest.skipUnless(BINARY.exists() and shutil.which('mcpx'), 'needs a release build and mcpx on PATH')
    def test_an_agent_bot_reaches_the_shop_through_mcpx_with_only_the_arm_guidance(self):
        record = code_mode_eval.run_one('agent-code', 'small', 0, self.out, self.args, self.skill)
        self.assertTrue(record['correct'], record)
        self.assertEqual(record['tools'], {'shell': 1})
        self.assertEqual(record['commands'], ['mcpx shop get_product --sku=P17'])
        self.assertEqual(record['shop_calls'], {'get_product': 1})
        self.assertEqual(record['model_requests'], 2)
        self.assertEqual(record['usage'], {'input_tokens': 20, 'cached_input_tokens': 8, 'output_tokens': 4})
        # The bot's instructions carry the arm's guidance and nothing from the person's own home.
        self.assertIn(code_mode_eval.CODE_HINT, self.server.requests[0]['instructions'])
        self.assertIn('mcpx - MCP tools as Unix commands', self.server.requests[0]['instructions'])

    @unittest.skipUnless(shutil.which('codex') and shutil.which('mcpx'), 'needs codex and mcpx on PATH')
    def test_codex_calls_the_shop_with_direct_tools_natively_and_through_mcpx(self):
        # A model Codex has no metadata for gets direct tools, not code mode.
        native = code_mode_eval.run_one('codex-native', 'small', 0, self.out, self.args, self.skill)
        self.assertTrue(native['correct'], native)
        self.assertEqual(native['tools'], {'mcp__shop.get_product': 1})
        self.assertEqual(native['shop_calls'], {'get_product': 1})
        self.assertEqual(native['model_requests'], 2)
        self.assertEqual(native['usage'], {'input_tokens': 20, 'cached_input_tokens': 8, 'output_tokens': 4})
        self.assertNotIn('mcpx - MCP tools', self.instructions())
        self.server.requests.clear()
        through = code_mode_eval.run_one('codex-mcpx', 'small', 0, self.out, self.args, self.skill)
        self.assertTrue(through['correct'], through)
        self.assertEqual(through['tools'], {'exec_command': 1})
        self.assertEqual(through['shop_calls'], {'get_product': 1})
        self.assertEqual(len(through['commands']), 1)
        self.assertIn('mcpx shop get_product --sku=P17', through['commands'][0])
        self.assertIn('mcpx - MCP tools as Unix commands', self.instructions())
        self.assertNotIn('mcp__shop', self.instructions())
        self.assertNotIn(code_mode_eval.CODE_HINT, self.instructions())

    @unittest.skipUnless(shutil.which('codex') and shutil.which('mcpx'), 'needs codex and mcpx on PATH')
    def test_codex_code_mode_counts_the_calls_its_scripts_make(self):
        self.args.model = 'gpt-6-luna'  # a model Codex runs in code mode
        native = code_mode_eval.run_one('codex-direct', 'small', 0, self.out, self.args, self.skill)
        self.assertTrue(native['correct'], native)
        self.assertEqual(native['tools'], {'functions.exec': 1})
        self.assertEqual(native['shop_calls'], {'get_product': 1})
        self.assertIn('tools.mcp__shop__get_product', native['commands'][0])
        self.assertIn(code_mode_eval.DIRECT_HINT, self.instructions())
        self.server.requests.clear()
        through = code_mode_eval.run_one('codex-mcpx', 'small', 0, self.out, self.args, self.skill)
        self.assertTrue(through['correct'], through)
        self.assertEqual(through['tools'], {'functions.exec': 1})
        self.assertEqual(through['shop_calls'], {'get_product': 1})
        # The script, and the shell command it ran from inside it.
        self.assertEqual(len(through['commands']), 2)
        self.assertTrue(any(c.startswith('[functions.exec]') for c in through['commands']), through['commands'])
        self.assertTrue(any('bash' in c and 'mcpx shop get_product --sku=P17' in c for c in through['commands']))


if __name__ == '__main__':
    unittest.main()
