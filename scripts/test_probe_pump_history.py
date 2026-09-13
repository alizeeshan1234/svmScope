"""Offline boundary tests for the experimental historical event extractor."""
import base64
import copy
import struct
import unittest

from probe_pump_history import DISCRIMINATOR, PROGRAM, decode_event, transaction_events


def event_bytes(buy):
    raw = bytearray(129)
    raw[:8] = DISCRIMINATOR
    struct.pack_into("<QQ", raw, 40, 10, 20)
    raw[56] = buy
    struct.pack_into("<QQQQ", raw, 97, 110, 180, 60, 130)
    return raw


def transaction():
    payload = base64.b64encode(event_bytes(1)).decode()
    return {"meta": {"err": None, "logMessages": [
        f"Program {PROGRAM} invoke [1]",
        f"Program data: {payload}",
        f"Program {PROGRAM} success",
    ]}}


class HistoricalEvents(unittest.TestCase):
    def test_buy_and_sell_preimages(self):
        self.assertEqual(decode_event(event_bytes(1))["candidate_pre"], {
            "virtual_sol": 100, "virtual_token": 200, "real_sol": 50, "real_token": 150,
        })
        self.assertEqual(decode_event(event_bytes(0))["candidate_pre"], {
            "virtual_sol": 120, "virtual_token": 160, "real_sol": 70, "real_token": 110,
        })

    def test_failed_transactions_do_not_supply_state(self):
        tx = transaction()
        tx["meta"]["err"] = {"InstructionError": [0, "InvalidArgument"]}
        self.assertEqual(transaction_events(tx), [])

    def test_payload_is_attributed_to_active_program(self):
        tx = transaction()
        self.assertEqual(len(transaction_events(tx)), 1)
        spoof = copy.deepcopy(tx)
        spoof["meta"]["logMessages"] = [
            line.replace(PROGRAM, "11111111111111111111111111111111")
            for line in spoof["meta"]["logMessages"]
        ]
        self.assertEqual(transaction_events(spoof), [])

    def test_truncated_or_inconsistent_stack_is_rejected(self):
        tx = transaction()
        tx["meta"]["logMessages"].pop()
        self.assertEqual(transaction_events(tx), [])
        tx = transaction()
        tx["meta"]["logMessages"][0] = f"Program {PROGRAM} invoke [2]"
        self.assertEqual(transaction_events(tx), [])

    def test_malformed_payloads_and_arithmetic(self):
        self.assertIsNone(decode_event(b"\0" * 129))
        self.assertIsNone(decode_event(event_bytes(1)[:128]))
        raw = event_bytes(2)
        with self.assertRaises(ValueError):
            decode_event(raw)
        raw = event_bytes(1)
        struct.pack_into("<Q", raw, 97, 0)
        with self.assertRaises(ValueError):
            decode_event(raw)


if __name__ == "__main__":
    unittest.main()
