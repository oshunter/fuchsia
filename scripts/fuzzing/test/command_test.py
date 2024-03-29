#!/usr/bin/env python2.7
# Copyright 2020 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import os
import sys
import unittest

import test_env
import lib.command as command
from test_case import TestCaseWithFuzzer


class CommandTest(TestCaseWithFuzzer):

    # Unit tests

    def test_list_fuzzers(self):
        # No match
        args = self.parse_args('list', 'no/match')
        command.list_fuzzers(args, self.factory)
        self.assertLogged('No matching fuzzers.')

        # Multiple matches
        args = self.parse_args('list', 'fake-package1')
        command.list_fuzzers(args, self.factory)
        self.assertLogged(
            'Found 3 matching fuzzers:',
            '  fake-package1/fake-target1',
            '  fake-package1/fake-target2',
            '  fake-package1/fake-target3',
        )

        # Exact match
        args = self.parse_args('list', 'fake-package1/fake-target1')
        command.list_fuzzers(args, self.factory)
        self.assertLogged(
            'Found 1 matching fuzzers:',
            '  fake-package1/fake-target1',
        )

    def test_start_fuzzer(self):
        name = str(self.fuzzer)

        # In the foreground
        output = 'output'
        self.host.mkdir(output)
        args = self.parse_args('start', '-f', '-o', output, name)
        command.start_fuzzer(args, self.factory)
        self.assertLogged(
            'Starting {}.'.format(self.fuzzer),
            'Outputs will be written to: {}'.format(output),
        )

        # In the background
        args = self.parse_args('start', '-o', output, name)
        command.start_fuzzer(args, self.factory)
        self.assertLogged(
            'Starting {}.'.format(self.fuzzer),
            'Outputs will be written to: {}'.format(output),
            'Check status with "fx fuzz check {}".'.format(self.fuzzer),
            'Stop manually with "fx fuzz stop {}".'.format(self.fuzzer),
        )
        cmd = [
            'python',
            sys.argv[0],
            'start',
            '--monitor',
            '--output',
            output,
            name,
        ]
        self.assertRan(*cmd)

        args = self.parse_args('start', '-m', '-o', output, name)
        command.start_fuzzer(args, self.factory)
        self.assertLogged(
            '{} has stopped.'.format(self.fuzzer),
            'Output written to: {}.'.format(output),
        )

    def test_check_fuzzer(self):
        # No name, none running
        args = self.parse_args('check')
        command.check_fuzzer(args, self.factory)
        self.assertLogged(
            'No fuzzers are running.',
            'Include \'name\' to check specific fuzzers.',
        )

        # Name provided, not installed
        fuzzer1 = self.create_fuzzer(
            'fake-package2/fake-target1', resolve=False)
        args = self.parse_args('check', 'fake-package2/fake-target1')
        command.check_fuzzer(args, self.factory)
        self.assertLogged(
            'fake-package2/fake-target1: NOT INSTALLED',
            '',
        )

        # No name, some running
        fuzzer1 = self.create_fuzzer('fake-package1/fake-target1')
        fuzzer2 = self.create_fuzzer('fake-package1/fake-target2')
        fuzzer3 = self.create_fuzzer('fake-package1/fake-target3')
        self.set_running(fuzzer1.executable_url)
        self.set_running(fuzzer3.executable_url)
        args = self.parse_args('check')
        command.check_fuzzer(args, self.factory)
        self.assertLogged(
            'fake-package1/fake-target1: RUNNING',
            '    Corpus size:  0 inputs / 0 bytes',
            '',
            'fake-package1/fake-target3: RUNNING',
            '    Corpus size:  0 inputs / 0 bytes',
            '',
        )

        # Name provided, running
        args = self.parse_args('check', 'fake-package1/fake-target3')
        command.check_fuzzer(args, self.factory)
        self.assertLogged(
            'fake-package1/fake-target3: RUNNING',
            '    Corpus size:  0 inputs / 0 bytes',
            '',
        )

        # Name provided, not running
        args = self.parse_args('check', 'fake-package1/fake-target2')
        command.check_fuzzer(args, self.factory)
        self.assertLogged(
            'fake-package1/fake-target2: STOPPED',
            '    Corpus size:  0 inputs / 0 bytes',
            '',
        )

        # Add some artifacts
        fuzzer = self.create_fuzzer('fake-package1/fake-target2')
        self.host.touch(os.path.join(fuzzer.output, 'crash-deadbeef'))
        self.host.touch(os.path.join(fuzzer.output, 'leak-feedface'))
        command.check_fuzzer(args, self.factory)
        self.assertLogged(
            'fake-package1/fake-target2: STOPPED',
            '    Corpus size:  0 inputs / 0 bytes',
            '    Artifacts:',
            '        {}/crash-deadbeef'.format(fuzzer.output),
            '        {}/leak-feedface'.format(fuzzer.output),
            '',
        )

    def test_stop_fuzzer(self):
        # Not running
        args = self.parse_args('stop', str(self.fuzzer))
        command.stop_fuzzer(args, self.factory)
        self.assertLogged('{} is already stopped.'.format(self.fuzzer))

        # Running
        self.set_running(self.fuzzer.executable_url)
        args = self.parse_args('stop', str(self.fuzzer))
        command.stop_fuzzer(args, self.factory)
        self.assertLogged('Stopping {}.'.format(self.fuzzer))

    def test_repro_units(self):
        unit = 'crash-deadbeef'
        self.host.touch(unit)
        args = self.parse_args('repro', str(self.fuzzer), unit)
        command.repro_units(args, self.factory)

    def test_analyze_fuzzer(self):
        args = self.parse_args('analyze', '-l', str(self.fuzzer))
        command.analyze_fuzzer(args, self.factory)

        # We shouldn't have copied anything
        self.assertFalse([cmd for cmd in self.host.processes if 'gs' in cmd])

        # Invalid corpus
        corpus1 = 'corpus1'
        corpus2 = 'corpus2'
        local_dict = 'local_dict'
        args = self.parse_args(
            'analyze', '-c', corpus1, '-c', corpus2, '-d', local_dict,
            '{}/{}'.format(self.fuzzer.package, self.fuzzer.executable))
        self.assertError(
            lambda: command.analyze_fuzzer(args, self.factory),
            'No such directory: {}'.format(corpus1))

        self.host.mkdir(corpus1)
        foo = os.path.join(corpus1, 'foo')
        bar = os.path.join(corpus1, 'bar')
        self.host.touch(foo)
        self.host.touch(bar)
        self.assertError(
            lambda: command.analyze_fuzzer(args, self.factory),
            'No such directory: {}'.format(corpus2))

        # Invalid dictionary
        self.host.mkdir(corpus2)
        baz = os.path.join(corpus2, 'baz')
        self.host.touch(baz)
        self.assertError(
            lambda: command.analyze_fuzzer(args, self.factory),
            'No such file: {}'.format(local_dict))

        self.host.touch(local_dict)
        with self.host.temp_dir() as temp_dir:
            # Make it appear as if something was retrieved from GCS.
            qux = os.path.join(temp_dir.pathname, 'qux')
            self.host.touch(qux)
        command.analyze_fuzzer(args, self.factory)
        gcs_url = 'gs://corpus.internal.clusterfuzz.com/libFuzzer/fuchsia_{}-{}'.format(
            self.fuzzer.package, self.fuzzer.executable)
        with self.host.temp_dir() as temp_dir:
            cmd = ['gsutil', '-m', 'cp', gcs_url + '/*', temp_dir.pathname]
            self.assertRan(*cmd)

        abspath = self.ns.data_abspath(self.corpus.nspaths[0])
        self.assertScpTo(bar, foo, abspath)
        self.assertScpTo(baz, abspath)
        self.assertEqual(
            self.dictionary.nspath, self.fuzzer.ns.data(local_dict))


if __name__ == '__main__':
    unittest.main()
