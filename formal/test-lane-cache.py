#!/usr/bin/env python3
"""The cache key must bind bounds, command, tools and scope bytes."""
import importlib.util
from pathlib import Path

path = Path(__file__).with_name("lane-cache.py")
spec = importlib.util.spec_from_file_location("lane_cache", path)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
key = module.action_key
baseline = key("triangle", ["bash", "lane.sh"], {"LOOM_MAX_BRANCHES": "200"}, {"rustc": "a"}, "inputs")
assert baseline == key("triangle", ["bash", "lane.sh"], {"LOOM_MAX_BRANCHES": "200"}, {"rustc": "a"}, "inputs")
assert baseline != key("triangle", ["bash", "lane.sh"], {"LOOM_MAX_BRANCHES": "1"}, {"rustc": "a"}, "inputs")
assert baseline != key("triangle", ["bash", "other.sh"], {"LOOM_MAX_BRANCHES": "200"}, {"rustc": "a"}, "inputs")
assert baseline != key("triangle", ["bash", "lane.sh"], {"LOOM_MAX_BRANCHES": "200"}, {"rustc": "b"}, "inputs")
assert baseline != key("triangle", ["bash", "lane.sh"], {"LOOM_MAX_BRANCHES": "200"}, {"rustc": "a"}, "changed")
