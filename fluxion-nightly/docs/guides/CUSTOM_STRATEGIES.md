# Custom External Strategies Guide

This guide explains how to create custom battery control strategies for FluxION using any programming language (Python, Go, Node.js, etc.) via the HTTP Plugin Protocol.

## Overview

FluxION's plugin architecture allows external strategies to participate in battery scheduling decisions alongside built-in strategies. External plugins communicate via HTTP/REST, making them language-agnostic.

### How It Works

1. **Registration**: Your external plugin registers with FluxION via REST API
2. **Evaluation**: FluxION sends evaluation requests to your plugin's callback URL
3. **Decision**: Your plugin returns a decision (charge, discharge, self-use, etc.)
4. **Priority Resolution**: FluxION merges decisions from all plugins using priority

```
┌─────────────────────────────────────────────────────────────┐
│                       FluxION Engine                        │
│  ┌─────────────────┐  ┌─────────────────┐  ┌─────────────┐ │
│  │ Winter Adaptive │  │ Winter Adaptive │  │ Your Custom │ │
│  │       V1        │  │       V2        │  │   Plugin    │ │
│  │  (priority 100) │  │  (priority 90)  │  │ (priority N)│ │
│  └────────┬────────┘  └────────┬────────┘  └──────┬──────┘ │
│           │                    │                   │        │
│           └────────────┬───────┴───────────────────┘        │
│                        ▼                                    │
│              ┌─────────────────────┐                        │
│              │   Plugin Manager    │                        │
│              │ (merges by priority)│                        │
│              └─────────┬───────────┘                        │
│                        ▼                                    │
│              ┌─────────────────────┐                        │
│              │  Schedule Executor  │                        │
│              └─────────────────────┘                        │
└─────────────────────────────────────────────────────────────┘
```

---

## Protocol Specification

### Registration

Register your plugin by POSTing to FluxION's API:

```
POST http://fluxion-host:8099/api/plugins/register
Content-Type: application/json

{
  "manifest": {
    "name": "my-python-strategy",
    "version": "1.0.0",
    "description": "Custom ML-based battery optimizer",
    "default_priority": 95,
    "enabled": true
  },
  "callback_url": "http://your-host:8100/evaluate"
}
```

**Response:**
```json
{
  "success": true,
  "plugin_id": "http:my-python-strategy",
  "error": null
}
```

### Evaluation Request

FluxION sends POST requests to your `callback_url` for each 15-minute block:

```json
{
  "block": {
    "block_start": "2025-01-02T14:00:00Z",
    "duration_minutes": 15,
    "price_czk_per_kwh": 3.45
  },
  "battery": {
    "current_soc_percent": 65.5,
    "capacity_kwh": 10.0,
    "max_charge_rate_kw": 5.0,
    "min_soc_percent": 10.0,
    "max_soc_percent": 100.0,
    "efficiency": 0.92,
    "wear_cost_czk_per_kwh": 0.15
  },
  "forecast": {
    "solar_kwh": 0.8,
    "consumption_kwh": 0.5,
    "grid_export_price_czk_per_kwh": 0.10
  },
  "all_blocks": [
    {"block_start": "2025-01-02T14:00:00Z", "duration_minutes": 15, "price_czk_per_kwh": 3.45},
    {"block_start": "2025-01-02T14:15:00Z", "duration_minutes": 15, "price_czk_per_kwh": 3.20},
    ...
  ],
  "historical": {
    "grid_import_today_kwh": 5.2,
    "consumption_today_kwh": 8.5
  },
  "backup_discharge_min_soc": 20.0
}
```

### Decision Response

Your plugin must respond with:

```json
{
  "block_start": "2025-01-02T14:00:00Z",
  "duration_minutes": 15,
  "mode": "ForceCharge",
  "reason": "Cheapest block in next 8 hours",
  "priority": 95,
  "strategy_name": "my-python-strategy",
  "confidence": 0.85,
  "expected_profit_czk": -1.25,
  "decision_uid": "mps:charge:cheapest_block"
}
```

**Fields:**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `block_start` | ISO8601 datetime | Yes | Must match request |
| `duration_minutes` | integer | Yes | Must match request (usually 15) |
| `mode` | string | Yes | One of: `SelfUse`, `ForceCharge`, `ForceDischarge`, `BackUpMode` |
| `reason` | string | Yes | Human-readable explanation |
| `priority` | integer (0-100) | Yes | Higher wins conflicts |
| `strategy_name` | string | No | Your strategy name (for logging) |
| `confidence` | float (0.0-1.0) | No | Confidence in decision (tiebreaker) |
| `expected_profit_czk` | float | No | Expected profit/cost (tiebreaker) |
| `decision_uid` | string | No | Unique ID for debugging |

### Operation Modes

| Mode | Description |
|------|-------------|
| `SelfUse` | Normal operation - battery assists household load |
| `ForceCharge` | Force charge from grid at maximum rate |
| `ForceDischarge` | Force discharge to grid/load at maximum rate |
| `BackUpMode` | Hold battery charge, prevent discharge |

---

## Priority System

### How Priority Works

1. **All enabled plugins** are evaluated for each 15-minute block
2. **Highest priority wins** - plugin with highest priority number (0-100) determines the mode
3. **Tiebreakers** (when priorities are equal):
   - Higher confidence wins
   - Higher expected_profit_czk wins

### Priority Guidelines

| Priority Range | Recommended Use |
|----------------|-----------------|
| 100 | Critical safety overrides (e.g., battery protection) |
| 90-99 | Primary strategies (built-in Winter Adaptive V1 uses 100) |
| 70-89 | Secondary strategies, experimental algorithms |
| 50-69 | Advisory strategies, logging-only |
| 0-49 | Fallback strategies |

### Strategy Competition Example

```
Block: 2025-01-02 02:00 (cheap overnight hour)

Winter-Adaptive V1 (priority 100): ForceCharge - "Overnight charging"
Winter-Adaptive V2 (priority 90):  ForceCharge - "Scheduled charge block"
Your ML Strategy (priority 85):    SelfUse     - "Model predicts low usage"

Winner: Winter-Adaptive V1 (ForceCharge) - highest priority
```

If you want your strategy to override built-in strategies, set priority > 100.

---

## Python Implementation

### Minimal Example

```python
#!/usr/bin/env python3
"""Minimal FluxION external strategy plugin."""

from flask import Flask, request, jsonify
from datetime import datetime
import requests

app = Flask(__name__)

FLUXION_HOST = "http://localhost:8099"
CALLBACK_URL = "http://localhost:8100/evaluate"

@app.route('/evaluate', methods=['POST'])
def evaluate():
    """Handle evaluation request from FluxION."""
    data = request.json

    block = data['block']
    battery = data['battery']
    price = block['price_czk_per_kwh']
    soc = battery['current_soc_percent']

    # Simple logic: charge when cheap, discharge when expensive
    avg_price = sum(b['price_czk_per_kwh'] for b in data['all_blocks']) / len(data['all_blocks'])

    if price < avg_price * 0.7 and soc < 90:
        mode = "ForceCharge"
        reason = f"Cheap price {price:.2f} < {avg_price*0.7:.2f}"
    elif price > avg_price * 1.3 and soc > 30:
        mode = "ForceDischarge"
        reason = f"Expensive price {price:.2f} > {avg_price*1.3:.2f}"
    else:
        mode = "SelfUse"
        reason = "Normal operation"

    return jsonify({
        "block_start": block['block_start'],
        "duration_minutes": block['duration_minutes'],
        "mode": mode,
        "reason": reason,
        "priority": 85,
        "strategy_name": "simple-price-strategy",
        "decision_uid": f"sps:{mode.lower()}:{int(price*100)}"
    })

@app.route('/health', methods=['GET'])
def health():
    """Health check endpoint."""
    return jsonify({"healthy": True, "name": "simple-price-strategy"})

def register_with_fluxion():
    """Register this plugin with FluxION."""
    payload = {
        "manifest": {
            "name": "simple-price-strategy",
            "version": "1.0.0",
            "description": "Simple price-based strategy",
            "default_priority": 85,
            "enabled": True
        },
        "callback_url": CALLBACK_URL
    }

    try:
        resp = requests.post(f"{FLUXION_HOST}/api/plugins/register", json=payload)
        if resp.status_code == 201:
            print(f"Registered successfully: {resp.json()}")
        else:
            print(f"Registration failed: {resp.status_code} - {resp.text}")
    except Exception as e:
        print(f"Could not connect to FluxION: {e}")

if __name__ == '__main__':
    register_with_fluxion()
    app.run(host='0.0.0.0', port=8100)
```

### Advanced Example with ML

```python
#!/usr/bin/env python3
"""Advanced FluxION strategy with ML-based predictions."""

from flask import Flask, request, jsonify
from datetime import datetime, timedelta
import numpy as np
from typing import List, Dict, Any
import requests
import pickle

app = Flask(__name__)

class MLStrategy:
    def __init__(self):
        self.name = "ml-optimizer"
        self.priority = 92
        # Load pre-trained model (example)
        # self.model = pickle.load(open('model.pkl', 'rb'))

    def analyze_price_patterns(self, blocks: List[Dict]) -> Dict[str, Any]:
        """Analyze price patterns in upcoming blocks."""
        prices = [b['price_czk_per_kwh'] for b in blocks]

        return {
            'mean': np.mean(prices),
            'std': np.std(prices),
            'min': np.min(prices),
            'max': np.max(prices),
            'percentile_25': np.percentile(prices, 25),
            'percentile_75': np.percentile(prices, 75),
            'spread': np.max(prices) - np.min(prices),
        }

    def find_arbitrage_opportunities(self, blocks: List[Dict], current_idx: int) -> Dict:
        """Find profitable charge/discharge windows."""
        prices = [b['price_czk_per_kwh'] for b in blocks]
        n = len(prices)

        best_charge_idx = None
        best_discharge_idx = None
        best_spread = 0

        # Look for charge opportunities followed by discharge
        for i in range(current_idx, min(current_idx + 32, n)):  # 8 hours ahead
            for j in range(i + 4, min(i + 48, n)):  # 1-12 hours after charge
                spread = prices[j] - prices[i]
                if spread > best_spread:
                    best_spread = spread
                    best_charge_idx = i
                    best_discharge_idx = j

        return {
            'charge_idx': best_charge_idx,
            'discharge_idx': best_discharge_idx,
            'spread': best_spread,
            'profitable': best_spread > 1.5  # > 1.5 CZK/kWh spread
        }

    def calculate_optimal_soc(self, hour: int, price_analysis: Dict) -> float:
        """Calculate optimal SOC target based on time and prices."""
        # Higher SOC targets during expensive hours
        if 6 <= hour <= 9 or 17 <= hour <= 21:  # Peak hours
            return 80.0
        elif 0 <= hour <= 5:  # Overnight charging window
            return 95.0
        else:
            return 60.0

    def evaluate(self, data: Dict) -> Dict:
        """Main evaluation logic."""
        block = data['block']
        battery = data['battery']
        all_blocks = data['all_blocks']

        block_time = datetime.fromisoformat(block['block_start'].replace('Z', '+00:00'))
        hour = block_time.hour
        price = block['price_czk_per_kwh']
        soc = battery['current_soc_percent']

        # Find current block index
        current_idx = 0
        for i, b in enumerate(all_blocks):
            if b['block_start'] == block['block_start']:
                current_idx = i
                break

        # Analyze patterns
        price_analysis = self.analyze_price_patterns(all_blocks[current_idx:])
        arbitrage = self.find_arbitrage_opportunities(all_blocks, current_idx)
        optimal_soc = self.calculate_optimal_soc(hour, price_analysis)

        # Decision logic
        mode = "SelfUse"
        reason = "Default operation"
        confidence = 0.5
        decision_uid = "ml:self_use:default"

        # Priority 1: Arbitrage opportunity
        if arbitrage['profitable'] and arbitrage['charge_idx'] == current_idx:
            if soc < 95:
                mode = "ForceCharge"
                reason = f"Arbitrage: charge now, spread={arbitrage['spread']:.2f} CZK"
                confidence = 0.9
                decision_uid = f"ml:charge:arbitrage:{int(arbitrage['spread']*100)}"

        elif arbitrage['profitable'] and arbitrage['discharge_idx'] == current_idx:
            if soc > 30:
                mode = "ForceDischarge"
                reason = f"Arbitrage: discharge now, spread={arbitrage['spread']:.2f} CZK"
                confidence = 0.9
                decision_uid = f"ml:discharge:arbitrage:{int(arbitrage['spread']*100)}"

        # Priority 2: Price below 25th percentile - charge
        elif price <= price_analysis['percentile_25'] and soc < optimal_soc:
            mode = "ForceCharge"
            reason = f"Low price ({price:.2f} <= P25={price_analysis['percentile_25']:.2f})"
            confidence = 0.8
            decision_uid = "ml:charge:low_percentile"

        # Priority 3: Price above 75th percentile - discharge
        elif price >= price_analysis['percentile_75'] and soc > 25:
            mode = "ForceDischarge" if price > price_analysis['mean'] * 1.5 else "SelfUse"
            reason = f"High price ({price:.2f} >= P75={price_analysis['percentile_75']:.2f})"
            confidence = 0.75
            decision_uid = "ml:discharge:high_percentile"

        # Priority 4: SOC management
        elif soc < 20:
            mode = "BackUpMode"
            reason = f"Low SOC protection ({soc:.1f}%)"
            confidence = 0.95
            decision_uid = "ml:backup:low_soc"

        return {
            "block_start": block['block_start'],
            "duration_minutes": block['duration_minutes'],
            "mode": mode,
            "reason": reason,
            "priority": self.priority,
            "strategy_name": self.name,
            "confidence": confidence,
            "expected_profit_czk": self._estimate_profit(mode, price, battery),
            "decision_uid": decision_uid
        }

    def _estimate_profit(self, mode: str, price: float, battery: Dict) -> float:
        """Estimate profit/cost for this decision."""
        energy_kwh = battery['max_charge_rate_kw'] * 0.25  # 15 minutes

        if mode == "ForceCharge":
            return -price * energy_kwh  # Cost to charge
        elif mode == "ForceDischarge":
            return price * energy_kwh * battery['efficiency']  # Revenue from discharge
        return 0.0

strategy = MLStrategy()

@app.route('/evaluate', methods=['POST'])
def evaluate():
    return jsonify(strategy.evaluate(request.json))

@app.route('/health', methods=['GET'])
def health():
    return jsonify({
        "healthy": True,
        "name": strategy.name,
        "version": "1.0.0"
    })

if __name__ == '__main__':
    # Register on startup
    requests.post("http://localhost:8099/api/plugins/register", json={
        "manifest": {
            "name": strategy.name,
            "version": "1.0.0",
            "description": "ML-based battery optimizer",
            "default_priority": strategy.priority,
            "enabled": True
        },
        "callback_url": "http://localhost:8100/evaluate"
    })
    app.run(host='0.0.0.0', port=8100)
```

### Requirements

```txt
# requirements.txt
flask>=2.0.0
requests>=2.25.0
numpy>=1.20.0
```

---

## Plugin Lifecycle

### Registration Flow

```
┌──────────────┐         ┌─────────────┐
│ Your Plugin  │         │   FluxION   │
└──────┬───────┘         └──────┬──────┘
       │                        │
       │  POST /api/plugins/register
       │  {manifest, callback_url}
       │───────────────────────►│
       │                        │
       │   201 Created          │
       │   {plugin_id}          │
       │◄───────────────────────│
       │                        │
       │  (Every 15 min block)  │
       │  POST /evaluate        │
       │  {EvaluationRequest}   │
       │◄───────────────────────│
       │                        │
       │  200 OK                │
       │  {BlockDecision}       │
       │───────────────────────►│
       │                        │
```

### Health Monitoring

FluxION tracks plugin health:

- **Timeout**: 5 seconds per evaluation (configurable)
- **Auto-disable**: After 3 consecutive failures
- **Re-enable**: Via API or by resetting failure count

```python
# Check plugin status
GET /api/plugins

# Response
{
  "plugins": [
    {
      "name": "http:my-strategy",
      "priority": 85,
      "enabled": true,
      "plugin_type": "external"
    }
  ],
  "count": 1
}
```

### Management API

```bash
# List all plugins
curl http://localhost:8099/api/plugins

# Update priority
curl -X PUT http://localhost:8099/api/plugins/http:my-strategy/priority \
  -H "Content-Type: application/json" \
  -d '{"priority": 95}'

# Enable/disable
curl -X PUT http://localhost:8099/api/plugins/http:my-strategy/enabled \
  -H "Content-Type: application/json" \
  -d '{"enabled": false}'

# Unregister (disables the plugin)
curl -X DELETE http://localhost:8099/api/plugins/http:my-strategy
```

---

## Deployment Patterns

### Pattern 1: Sidecar Container

Run your strategy alongside FluxION in Docker:

```yaml
# docker-compose.yml
version: '3.8'
services:
  fluxion:
    image: fluxion:latest
    ports:
      - "8099:8099"
    environment:
      - SUPERVISOR_TOKEN=${SUPERVISOR_TOKEN}

  my-strategy:
    build: ./my-strategy
    ports:
      - "8100:8100"
    environment:
      - FLUXION_HOST=http://fluxion:8099
      - CALLBACK_URL=http://my-strategy:8100/evaluate
    depends_on:
      - fluxion
```

### Pattern 2: Home Assistant Add-on

Create a separate HA add-on that registers with FluxION:

```yaml
# config.yaml (HA add-on)
name: "My FluxION Strategy"
description: "Custom battery optimization strategy"
version: "1.0.0"
slug: "fluxion-my-strategy"
init: false
arch:
  - amd64
  - aarch64
ports:
  8100/tcp: 8100
options:
  fluxion_host: "http://homeassistant.local:8099"
  priority: 85
```

### Pattern 3: Remote Server

Run your strategy on a separate machine (e.g., with GPU for ML):

```python
# Ensure FluxION can reach your server
CALLBACK_URL = "http://your-server.local:8100/evaluate"

# Use proper error handling for network issues
@app.route('/evaluate', methods=['POST'])
def evaluate():
    try:
        return jsonify(strategy.evaluate(request.json))
    except Exception as e:
        # Return safe fallback on error
        return jsonify({
            "block_start": request.json['block']['block_start'],
            "duration_minutes": 15,
            "mode": "SelfUse",
            "reason": f"Strategy error: {str(e)}",
            "priority": 0,  # Low priority = let other strategies win
        })
```

---

## Best Practices

### 1. Always Return Valid Responses

Even on errors, return a valid `BlockDecision`:

```python
@app.route('/evaluate', methods=['POST'])
def evaluate():
    try:
        return jsonify(do_evaluation(request.json))
    except Exception as e:
        # Fallback to safe mode
        return jsonify({
            "block_start": request.json['block']['block_start'],
            "duration_minutes": request.json['block']['duration_minutes'],
            "mode": "SelfUse",
            "reason": f"Error: {str(e)}",
            "priority": 0,
            "strategy_name": "my-strategy",
            "decision_uid": "error:fallback"
        })
```

### 2. Use Meaningful Decision UIDs

Decision UIDs help with debugging:

```python
# Good: descriptive, hierarchical
"ml:charge:arbitrage:spread_250"
"ml:backup:low_soc:15pct"

# Bad: generic
"decision_1"
"charge"
```

### 3. Consider Battery Wear

Include battery wear cost in profit calculations:

```python
def estimate_cycle_cost(battery, energy_kwh):
    """Estimate battery degradation cost."""
    return energy_kwh * battery['wear_cost_czk_per_kwh']

def should_arbitrage(charge_price, discharge_price, battery):
    """Check if arbitrage is profitable after wear."""
    energy = battery['max_charge_rate_kw'] * 0.25
    gross_profit = (discharge_price - charge_price) * energy * battery['efficiency']
    wear_cost = estimate_cycle_cost(battery, energy * 2)  # Charge + discharge
    return gross_profit > wear_cost
```

### 4. Respect Timeout Constraints

Keep evaluation fast (< 5 seconds):

```python
import signal

class TimeoutError(Exception):
    pass

def timeout_handler(signum, frame):
    raise TimeoutError("Evaluation timeout")

@app.route('/evaluate', methods=['POST'])
def evaluate():
    signal.signal(signal.SIGALRM, timeout_handler)
    signal.alarm(4)  # 4 second timeout
    try:
        result = do_evaluation(request.json)
        signal.alarm(0)
        return jsonify(result)
    except TimeoutError:
        return jsonify(fallback_decision(request.json))
```

### 5. Log Decisions for Analysis

```python
import logging
from datetime import datetime

logging.basicConfig(level=logging.INFO, filename='decisions.log')

def log_decision(request, decision):
    logging.info(f"{datetime.now().isoformat()} | "
                 f"block={request['block']['block_start']} | "
                 f"price={request['block']['price_czk_per_kwh']:.2f} | "
                 f"soc={request['battery']['current_soc_percent']:.1f}% | "
                 f"mode={decision['mode']} | "
                 f"reason={decision['reason']}")
```

---

## Troubleshooting

### Plugin Not Receiving Requests

1. Check registration succeeded:
   ```bash
   curl http://localhost:8099/api/plugins | jq
   ```

2. Verify callback URL is reachable from FluxION:
   ```bash
   # From FluxION container/host
   curl http://your-plugin:8100/health
   ```

3. Check FluxION logs for errors:
   ```bash
   docker logs fluxion 2>&1 | grep -i plugin
   ```

### Plugin Auto-Disabled

After 3 consecutive failures, plugins are auto-disabled. To re-enable:

```bash
curl -X PUT http://localhost:8099/api/plugins/http:my-strategy/enabled \
  -H "Content-Type: application/json" \
  -d '{"enabled": true}'
```

### Priority Not Working

Ensure your priority is higher than competing strategies:
- Winter-Adaptive V1: priority 100 (default)
- Winter-Adaptive V2: priority 90 (default)

Set `priority > 100` to override built-in strategies.

---

## Security Considerations

1. **Network Isolation**: Run plugins in the same Docker network as FluxION
2. **Input Validation**: Validate all incoming data before processing
3. **No Sensitive Data**: Don't log or store sensitive information
4. **Rate Limiting**: FluxION only calls once per 15-min block, but protect against abuse

---

## Strategy Management & Configuration

### Current Approach: API-Based Registration

External strategies register themselves via the REST API. This is flexible but requires:
- External service must be running before registration
- Registration is lost on FluxION restart (plugin must re-register)
- No visibility in FluxION's main configuration

### Future: Config-Based External Strategies

A planned enhancement will allow defining external strategies in FluxION's configuration:

```yaml
# Example future configuration
strategies:
  winter_adaptive:
    enabled: true
    priority: 100

  winter_adaptive_v2:
    enabled: true
    priority: 90

  # External strategies defined in config
  external:
    - name: "my-python-strategy"
      callback_url: "http://localhost:8100/evaluate"
      priority: 85
      enabled: true
      timeout_ms: 5000
      max_failures: 3

    - name: "ml-optimizer"
      callback_url: "http://ml-server:8200/evaluate"
      priority: 92
      enabled: true
```

This would provide:
- Persistent configuration across restarts
- Web UI integration for enable/disable
- Centralized priority management

### Recommended Approach Today

Until config integration is complete, use this pattern:

1. **Auto-registration on startup**: Have your external strategy register itself when it starts
2. **Health endpoint**: Implement `/health` so FluxION can verify availability
3. **Graceful degradation**: Return low-priority fallback decisions on errors
4. **Priority coordination**: Document your strategy's priority relative to built-ins

```python
# Startup registration pattern
def main():
    # Try to register multiple times on startup
    for attempt in range(5):
        if register_with_fluxion():
            break
        time.sleep(10)  # Wait 10s between attempts

    # Start server regardless
    app.run(host='0.0.0.0', port=8100)
```

### Priority Guidelines for Custom Strategies

When setting priority for your custom strategy:

| Scenario | Recommended Priority | Reason |
|----------|---------------------|--------|
| **Override everything** | 101+ | Higher than all built-in strategies |
| **Primary custom strategy** | 95 | Below V1 (100) but above V2 (90) |
| **Experimental / testing** | 80-89 | Can be overridden by built-ins |
| **Advisory only** | 50-70 | Suggestions, not commands |
| **Logging / monitoring** | 0-30 | Never wins, just observes |

### Multiple Custom Strategies

You can run multiple external strategies simultaneously:

```
┌────────────────────────────────────────────────────────────┐
│                    Priority Resolution                      │
├────────────────────────────────────────────────────────────┤
│  Winter-Adaptive V1     │ priority: 100 │ Built-in        │
│  Your ML Strategy       │ priority: 95  │ External HTTP   │
│  Winter-Adaptive V2     │ priority: 90  │ Built-in        │
│  Your Simple Strategy   │ priority: 85  │ External HTTP   │
│  Your Monitor Strategy  │ priority: 10  │ External HTTP   │
└────────────────────────────────────────────────────────────┘
                              │
                              ▼
              Highest priority enabled plugin wins
```

Each strategy:
- Receives the same `EvaluationRequest`
- Returns its own `BlockDecision`
- Competes based on priority

This allows:
- A/B testing between strategies
- Fallback chains (if high-priority fails, lower takes over)
- Monitoring strategies that log but don't control

---

## Integration Status

### Currently Implemented

- REST API for plugin registration (`POST /api/plugins/register`)
- REST API for plugin management (`GET/PUT/DELETE /api/plugins/*`)
- HTTP plugin evaluation with timeout and failure tracking
- Priority-based decision merging
- Auto-disable after consecutive failures

### Not Yet Implemented

- Plugin API not connected in main.rs (passes `None` to web server)
- No config file integration for external strategies
- No Web UI for managing external plugins
- No persistent plugin storage across restarts

### Workaround

Until full integration, external strategies should:

1. Self-register on startup
2. Re-register periodically or on connection failure
3. Handle FluxION unavailability gracefully

```python
import threading
import time

def keep_registered():
    """Background thread to maintain registration."""
    while True:
        try:
            # Check if still registered
            resp = requests.get(f"{FLUXION_HOST}/api/plugins")
            plugins = resp.json().get('plugins', [])

            my_plugin = next(
                (p for p in plugins if p['name'] == 'http:my-strategy'),
                None
            )

            if not my_plugin or not my_plugin['enabled']:
                register_with_fluxion()

        except Exception as e:
            logger.warning(f"Registration check failed: {e}")
            register_with_fluxion()

        time.sleep(60)  # Check every minute

# Start background registration keeper
threading.Thread(target=keep_registered, daemon=True).start()
```

---

## Further Reading

- [FluxION Architecture](../architecture/ARCHITECTURE.md)
- [Winter Adaptive Strategy Logic](../WINTER_ADAPTIVE_LOGIC.md)
- [Configuration Guide](./CONFIGURATION.md)
- [Testing Guide](./TESTING.md)
