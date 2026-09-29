"""A synthetic shop behind a stdio MCP server, for the code-mode evaluation.

Customers, products, orders, support tickets, staff and service logs are
generated from a fixed seed, so every task has one checkable answer. Run it to serve MCP on stdin/stdout (newline-delimited JSON-RPC 2.0);
`--log FILE` appends one line per tool call, so every harness's calls are
counted at the server, however the model made them:

    python3 bench/code_mode_shop.py --log calls.log

Standard library only, so the same file runs under Codex and under mcpx.
"""
import json
import random
import sys
from datetime import date, timedelta

SEED = 20260929
REGIONS = ('EU', 'NA', 'APAC', 'LATAM')
CATEGORIES = ('garden', 'kitchen', 'tools', 'toys', 'books', 'outdoor')
SERVICES = ('billing', 'auth', 'search')
LOG_DAYS = [(date(2025, 6, 1) + timedelta(days=n)).isoformat() for n in range(7)]
PAGE_MAX = 50


def build(seed=SEED):
    rng = random.Random(seed)
    products = {}
    for n in range(1, 49):
        sku = f'P{n:02d}'
        products[sku] = {'sku': sku, 'name': f'Item {n}', 'category': CATEGORIES[(n * 7) % len(CATEGORIES)],
                         'price_cents': rng.randrange(299, 19999)}
    customers, orders = {}, {}
    order_id = 0
    start = date(2024, 1, 1)
    for n in range(1, 241):
        cid = f'C{n:04d}'
        customers[cid] = {'id': cid, 'name': f'Customer {n}', 'email': f'customer{n}@example.test',
                          'region': REGIONS[rng.randrange(len(REGIONS))]}
        mine = []
        for _ in range(rng.randrange(0, 13)):
            order_id += 1
            items = []
            for _ in range(rng.randrange(1, 5)):
                sku = f'P{rng.randrange(1, 49):02d}'
                items.append({'sku': sku, 'qty': rng.randrange(1, 6), 'unit_price_cents': products[sku]['price_cents']})
            day = start + timedelta(days=rng.randrange(0, 731))
            mine.append({'id': f'O{order_id:05d}', 'date': day.isoformat(), 'items': items})
        orders[cid] = sorted(mine, key=lambda o: o['date'])
    employees = {}
    for n in range(1, 21):
        eid = f'E{n:02d}'
        employees[eid] = {'id': eid, 'name': f'Staff {n}', 'manager_id': None if n <= 3 else f'E{rng.randrange(1, 4):02d}'}
    tickets, ticket_id = {}, 0
    for cid in customers:
        for _ in range(rng.randrange(0, 4)):
            ticket_id += 1
            tid = f'T{ticket_id:04d}'
            opened = start + timedelta(days=rng.randrange(0, 731))
            tickets[tid] = {'id': tid, 'customer_id': cid, 'status': rng.choice(('open', 'open', 'closed')),
                            'created_at': f'{opened.isoformat()}T{rng.randrange(0, 24):02d}:{rng.randrange(0, 60):02d}:00Z',
                            'assignee_id': f'E{rng.randrange(4, 21):02d}', 'subject': f'Question {ticket_id}'}
    logs = {}
    words = ('request', 'retry', 'queue', 'cache', 'upstream', 'invoice', 'session', 'index')
    for service in SERVICES:
        for day in LOG_DAYS:
            lines = []
            for n in range(1500):
                level = rng.choices(('INFO', 'WARN', 'ERROR', 'DEBUG'), (70, 12, 8, 10))[0]
                kind = rng.random()
                if kind < 0.15:
                    message = f'{rng.choice(words)} timeout after {rng.randrange(100, 30000)} ms'
                elif kind < 0.2:
                    message = f'{rng.choice(words)} timed out waiting for lock'
                else:
                    message = f'{rng.choice(words)} handled id={rng.randrange(10**6)} in {rng.randrange(1, 900)} ms'
                seconds = n * 57 % 86400
                lines.append(f'{day}T{seconds // 3600:02d}:{seconds // 60 % 60:02d}:{seconds % 60:02d}Z '
                             f'{level} {service} {message}')
            logs[(service, day)] = lines
    return {'products': products, 'customers': customers, 'orders': orders, 'employees': employees,
            'tickets': tickets, 'logs': logs}


SHOP = build()

# A customer with at least two open tickets, for the chain task.
CHAIN_CUSTOMER = next(c for c in SHOP['customers'].values()
                      if sum(t['customer_id'] == c['id'] and t['status'] == 'open'
                             for t in SHOP['tickets'].values()) >= 2)
SMALL_SKU = 'P17'

TASKS = {
    'fanout': 'Using the shop tools, find the customer in the EU region who spent the most on products in the '
              'garden category in orders dated in 2025. Spending is quantity times unit price. Report the '
              'customer id and the amount in cents.',
    'bigdata': 'Using the shop tools, count the ERROR lines the billing service logged on 2025-06-03 whose '
               'message mentions a timeout (either "timeout" or "timed out").',
    'chain': f'Using the shop tools: the customer with email {CHAIN_CUSTOMER["email"]} has open support '
             'tickets. Find the employee assigned to their most recently created open ticket, and report '
             "that employee's manager id.",
    'small': f'Using the shop tools, report the category and the price in cents of product {SMALL_SKU}.',
}
ANSWER_FORMAT = {
    'fanout': 'ANSWER: <customer id> <cents>',
    'bigdata': 'ANSWER: <count>',
    'chain': 'ANSWER: <manager id>',
    'small': 'ANSWER: <category> <cents>',
}


def expected(shop=SHOP):
    garden = {s for s, p in shop['products'].items() if p['category'] == 'garden'}
    spend = {}
    for cid, c in shop['customers'].items():
        if c['region'] != 'EU':
            continue
        spend[cid] = sum(i['qty'] * i['unit_price_cents'] for o in shop['orders'][cid]
                         if o['date'].startswith('2025') for i in o['items'] if i['sku'] in garden)
    best = max(spend.values())
    top = [cid for cid, v in spend.items() if v == best]
    assert len(top) == 1, 'the fan-out task needs one winner'
    errors = sum(1 for line in shop['logs'][('billing', '2025-06-03')]
                 if line.split(' ', 2)[1] == 'ERROR' and ('timeout' in line or 'timed out' in line))
    opened = sorted((t for t in shop['tickets'].values()
                     if t['customer_id'] == CHAIN_CUSTOMER['id'] and t['status'] == 'open'),
                    key=lambda t: t['created_at'])
    manager = shop['employees'][opened[-1]['assignee_id']]['manager_id']
    product = shop['products'][SMALL_SKU]
    return {'fanout': f'{top[0]} {best}', 'bigdata': str(errors), 'chain': manager,
            'small': f'{product["category"]} {product["price_cents"]}'}


def answer_of(text):
    """The last `ANSWER:` line of a reply, normalized; None when there is none."""
    found = None
    for line in text.splitlines():
        line = line.strip().strip('`*').strip()
        if line.upper().startswith('ANSWER:'):
            found = ' '.join(line[7:].replace(',', ' ').split()).strip()
    return found


def correct(task, text):
    got = answer_of(text or '')
    return got is not None and got.lower() == expected()[task].lower()


def _schema(properties, required=()):
    return {'type': 'object', 'properties': properties, 'required': list(required), 'additionalProperties': False}


STR, INT = {'type': 'string'}, {'type': 'integer'}
TOOLS = [
    ('list_customers', 'List customers, one page at a time, optionally in one region (EU, NA, APAC, LATAM). '
     'Returns customers with id, name, email and region, plus page and pages.',
     _schema({'page': {**INT, 'minimum': 1}, 'page_size': {**INT, 'minimum': 1, 'maximum': PAGE_MAX},
              'region': STR})),
    ('get_customer', 'Get one customer by id.', _schema({'customer_id': STR}, ['customer_id'])),
    ('find_customer_by_email', 'Find a customer by email address.', _schema({'email': STR}, ['email'])),
    ('list_orders', "List a customer's orders with their items (sku, qty, unit_price_cents) and date.",
     _schema({'customer_id': STR}, ['customer_id'])),
    ('list_products', 'List every product with sku, name, category and price_cents.', _schema({})),
    ('get_product', 'Get one product by sku.', _schema({'sku': STR}, ['sku'])),
    ('list_tickets', "List a customer's support tickets: id, status, created_at, assignee_id, subject.",
     _schema({'customer_id': STR}, ['customer_id'])),
    ('get_ticket', 'Get one support ticket by id.', _schema({'ticket_id': STR}, ['ticket_id'])),
    ('get_employee', 'Get one employee by id: name and manager_id.', _schema({'employee_id': STR}, ['employee_id'])),
    ('search_logs', 'Return every log line a service (billing, auth, search) wrote on a day (YYYY-MM-DD). '
     'Lines are "TIME LEVEL SERVICE MESSAGE".',
     _schema({'service': STR, 'date': STR}, ['service', 'date'])),
]


class ToolError(Exception):
    pass


def call(name, args, shop=SHOP):
    def need(key, table):
        value = args.get(key)
        if value not in shop[table]:
            raise ToolError(f'unknown {key}: {value}')
        return value
    if name == 'list_customers':
        rows = [c for c in shop['customers'].values() if args.get('region') in (None, c['region'])]
        size = min(int(args.get('page_size', 20)), PAGE_MAX)
        page = int(args.get('page', 1))
        pages = max(1, -(-len(rows) // size))
        return {'customers': rows[(page - 1) * size:page * size], 'page': page, 'pages': pages}
    if name == 'get_customer':
        return shop['customers'][need('customer_id', 'customers')]
    if name == 'find_customer_by_email':
        match = [c for c in shop['customers'].values() if c['email'] == args.get('email')]
        if not match:
            raise ToolError('no customer with that email')
        return match[0]
    if name == 'list_orders':
        return {'orders': shop['orders'][need('customer_id', 'customers')]}
    if name == 'list_products':
        return {'products': list(shop['products'].values())}
    if name == 'get_product':
        return shop['products'][need('sku', 'products')]
    if name == 'list_tickets':
        cid = need('customer_id', 'customers')
        return {'tickets': [t for t in shop['tickets'].values() if t['customer_id'] == cid]}
    if name == 'get_ticket':
        return shop['tickets'][need('ticket_id', 'tickets')]
    if name == 'get_employee':
        return shop['employees'][need('employee_id', 'employees')]
    if name == 'search_logs':
        lines = shop['logs'].get((args.get('service'), args.get('date')))
        if lines is None:
            raise ToolError('no logs for that service and date')
        return '\n'.join(lines)
    raise ToolError(f'unknown tool: {name}')


def respond(message, log=None):
    method, mid = message.get('method'), message.get('id')
    if mid is None:
        return None
    if method == 'initialize':
        version = (message.get('params') or {}).get('protocolVersion', '2025-06-18')
        result = {'protocolVersion': version, 'capabilities': {'tools': {}},
                  'serverInfo': {'name': 'shop', 'version': '1'}}
    elif method == 'tools/list':
        result = {'tools': [{'name': n, 'description': d, 'inputSchema': s} for n, d, s in TOOLS]}
    elif method == 'tools/call':
        params = message.get('params') or {}
        if log:
            with open(log, 'a') as f:
                f.write(f"{params.get('name')}\n")
        try:
            value = call(params.get('name'), params.get('arguments') or {})
            text = value if isinstance(value, str) else json.dumps(value, separators=(',', ':'))
            result = {'content': [{'type': 'text', 'text': text}], 'isError': False}
        except ToolError as error:
            result = {'content': [{'type': 'text', 'text': str(error)}], 'isError': True}
    elif method == 'ping':
        result = {}
    else:
        return {'jsonrpc': '2.0', 'id': mid, 'error': {'code': -32601, 'message': f'unknown method {method}'}}
    return {'jsonrpc': '2.0', 'id': mid, 'result': result}


def serve(read=sys.stdin, write=sys.stdout, log=None):
    for line in read:
        if not line.strip():
            continue
        reply = respond(json.loads(line), log)
        if reply is not None:
            write.write(json.dumps(reply) + '\n')
            write.flush()


if __name__ == '__main__':
    serve(log=sys.argv[sys.argv.index('--log') + 1] if '--log' in sys.argv else None)
