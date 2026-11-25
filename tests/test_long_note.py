"""Exercise the qualification verdicts without claiming device measurements."""
import unittest

from long_note import Breach, Missing, check_slots, matrix, tails, validate


def example(size=0, action='append'):
    req = dict(profile='mac4k120', action=action, case='A', size=size, run=0, build='candidate',
               samples=1, warmups=100)
    device = dict(platform='macos', machine='test', driver='test', os='test', power_mode='AC',
                  pressure_start='normal', replay_hz=240, width=3840, height=2160, refresh_hz=120,
                  ram_bytes=8 * 2**30, external=True, integrated_gpu=True)
    row = dict(index=0, edit_id=1, presented_edit_id=1, frame_id=12, input_ns=0, commit_ns=1,
               submit_ns=2, gpu_done_ns=3, presented_ns=4, inspected=8, regenerated=12,
               uploaded_bytes=144, submitted_vertices=1200, gpu_ns=1, cpu_work_ns=1,
               drawable_wait_ns=0, coalesced=0, index_visits=0, appended_vertices=12,
               appended_vertex_bytes=144, deferred_ns=0)
    data = dict(device=device, commit='unit-test', release=True, warmups_completed=100,
                fixture_digest='fixture', viewport_digest='viewport', before_digest='state',
                restored_digest='state', actual_after_digest='edit', expected_after_digest='edit',
                observations=[row], active_windows=[dict(first_slot=12, last_slot=12,
                                                         presented_slots=[12])],
                memory=dict(peak_rss_bytes=1, post_close_rss_bytes=1, cache_bytes=0,
                            resident_pages=1))
    return req, data


class AcceptanceTests(unittest.TestCase):
    def test_nearest_rank_and_deadline_boundary(self):
        self.assertEqual(tails(range(1, 1001))['p99'], 990)
        window = dict(first_slot=1, last_slot=100, presented_slots=list(range(1, 100)))
        self.assertEqual(check_slots([window]), (1, 100))
        window['presented_slots'].pop()
        with self.assertRaises(Breach):
            check_slots([window])

    def test_control_and_constant_visible_work(self):
        controls = {}
        validate(*example(), controls)
        req, data = example(1000000)
        validate(req, data, controls)
        data['observations'][0]['inspected'] += 65
        with self.assertRaises(Breach):
            validate(req, data, controls)

    def test_upload_and_deferred_work_are_attributed_to_the_edit(self):
        controls = {}
        validate(*example(), controls)
        req, data = example(10000)
        data['observations'][0]['uploaded_bytes'] += 145
        with self.assertRaises(Breach):
            validate(req, data, controls)

    def test_missing_evidence_and_missing_frame_differ(self):
        req, data = example()
        row = data['observations'][0]
        del row['presented_edit_id']
        with self.assertRaises(Missing):
            validate(req, data, {})
        row['presented_edit_id'] = None
        with self.assertRaises(Breach):
            validate(req, data, {})

    def test_slow_edits_and_wrong_displays(self):
        req, data = example()
        data['observations'][0]['presented_ns'] = 17_000_000
        with self.assertRaises(Breach):
            validate(req, data, {})
        data['device']['external'] = False
        with self.assertRaises(Missing):
            validate(req, data, {})

    def test_blank_erase_and_round_trip(self):
        req, data = example(action='blank_erase')
        with self.assertRaises(Breach):
            validate(req, data, {})
        data['observations'][0].update(regenerated=0, uploaded_bytes=0)
        validate(req, data, {})
        data['restored_digest'] = 'different'
        with self.assertRaises(Breach):
            validate(req, data, {})

    def test_swap_growth_and_reopen(self):
        req, data = example(size=100, action='traverse')
        req.update(case='B', profile='igpu8')
        data.update(steps=list(range(20)) + [0], reopened_digest='state', swap_out_bytes=[1, 2, 2])
        validate(req, data, {})
        data['swap_out_bytes'] = [1, 2, 3]
        with self.assertRaises(Breach):
            validate(req, data, {})

    def test_matrix_sizes_and_actions(self):
        self.assertEqual([name for name, _, _ in matrix()], list('ABCDE'))
        self.assertIn(1000000, matrix()[0][1])
        self.assertIn(16384, matrix()[3][1])


if __name__ == '__main__':
    unittest.main()
