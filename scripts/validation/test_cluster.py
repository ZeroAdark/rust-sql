import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from cluster import Cluster


class ClusterEnvironmentTests(unittest.TestCase):
    def test_host_allocator_is_stripped_and_only_explicit_selection_is_forwarded(self):
        with tempfile.TemporaryDirectory() as directory, patch('os.geteuid', return_value=1000), \
             patch.dict(os.environ, {'PGRUST_ALLOCATOR': 'hearth', 'PGHOST': 'wrong-host'}):
            default = Cluster('/unused', directory)
            selected = Cluster('/unused', directory, allocator='mimalloc')
            self.assertNotIn('PGRUST_ALLOCATOR', default.env)
            self.assertEqual(selected.env['PGRUST_ALLOCATOR'], 'mimalloc')
            self.assertEqual(selected.env['PGHOST'], str(selected.socket))

    def test_invalid_or_native_allocator_selection_leaves_no_cluster(self):
        with tempfile.TemporaryDirectory() as directory, patch('os.geteuid', return_value=1000):
            for kwargs in ({'allocator': 'unknown'}, {'allocator': 'mimalloc', 'reference': True}):
                with self.subTest(kwargs=kwargs), self.assertRaises(ValueError):
                    Cluster('/unused', directory, **kwargs)
            self.assertEqual(list(Path(directory).iterdir()), [])
