#!/usr/bin/env python2.7
# Copyright 2020 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest
import sys

import test_env
import lib.args
from factory_fake import FakeFactory
from process_fake import FakeProcess
from StringIO import StringIO


class TestCaseWithIO(unittest.TestCase):

    # Unit test "constructor" and "destructor"

    def setUp(self):
        sys.stdin = StringIO()
        sys.stdout = StringIO()
        sys.stderr = StringIO()

    def tearDown(self):
        sys.stdin = sys.__stdin__
        sys.stdout = sys.__stdout__
        sys.stderr = sys.__stderr__

    # Unit test utilities

    def set_input(self, *lines):
        sys.stdin.truncate(0)
        for line in lines:
            sys.stdin.write(line)
            sys.stdin.write('\n')
        sys.stdin.flush()
        sys.stdin.seek(0)

    # Unit test assertions

    def _assert_io_equals(self, io, *lines):
        io.seek(0)
        self.assertEqual(io.read().split('\n')[:-1], list(lines))
        io.truncate(0)

    def assertOut(self, *lines):
        self._assert_io_equals(sys.stdout, *lines)

    def assertErr(self, *lines):
        self._assert_io_equals(sys.stderr, *lines)


class TestCaseWithFactory(TestCaseWithIO):
    """TestCase that provides common test context, utilities, and assertions."""

    # Unit test "constructor"

    def setUp(self):
        super(TestCaseWithFactory, self).setUp()
        self._factory = None
        self._next_pid = 10000

    # Unit test context, as aliases to the Factory.

    @property
    def factory(self):
        """The associated FakeFactory object."""
        if not self._factory:
            self._factory = FakeFactory()
        return self._factory

    @property
    def cli(self):
        """The associated CLI object."""
        return self.factory.cli

    @property
    def parser(self):
        """The associated ArgParser object."""
        return self.factory.parser

    @property
    def buildenv(self):
        """The associated BuildEnv object."""
        return self.factory.buildenv

    @property
    def device(self):
        """The associated Device object."""
        return self.factory.device

    @property
    def fuzzer(self):
        """The most recently created FakeFuzzer object."""
        return self.factory.fuzzer

    # Unit test utilities

    def _ssh_cmd(self, args):
        """Returns the command line arguments for an SSH commaned."""
        return ['ssh'] + self.device.ssh_opts() + [self.device.addr] + args

    def _scp_cmd(self, args):
        return ['scp'] + self.device.ssh_opts() + args

    def get_process(self, args, ssh=False):
        cmd = self._ssh_cmd(args) if ssh else args
        return self.cli.create_process(cmd)

    def parse_args(self, *args):
        return self.parser.parse_args(args)

    def create_fuzzer(self, *args):
        args = self.parse_args(*args)
        return self.factory.create_fuzzer(args, self.device)

    def set_outputs(
            self, args, outputs, start=None, end=None, reset=True, ssh=False):
        """Sets what will be returned from the stdout of a fake process.

        Providing a start and/or end will schedule the output to be added and/or
        removed, respectively, at a later time; see FakeProcess.schedule.
        Setting reset to True will replace any e4xisting output for the command.
        Setting ssh to true will automatically add the necessary SSH arguments.
        """
        process = self.get_process(args, ssh=ssh)
        if reset:
            process.clear()
        process.schedule('\n'.join(outputs), start=start, end=end)

    def set_running(self, package, executable, refresh=True, duration=None):
        """Marks a packaged executable as running on device.

        If refresh is True, this will cause the device to refresh its PIDs.
        If a duration is provided, the package executable will stop running
        after the given duration.
        """
        pid = self._next_pid
        self._next_pid += 1

        cmd = ['cs']
        output = '  {}.cmx[{}]: fuchsia-pkg://fuchsia.com/{}#meta/{}.cmx'.format(
            executable, pid, package, executable)
        end = None if not duration else self.cli.elapsed + duration
        self.set_outputs(cmd, [output], end=end, reset=False, ssh=True)

        self.device.getpid(package, executable, refresh)
        return pid

    # Unit test assertions

    def assertLogged(self, *logs):
        """Asserts logs were generated by calls to cli.echo or cli.error."""
        self.assertOut(*logs)

    def assertError(self, expr, *logs):
        assert logs, 'Missing error message.'
        logs = ['ERROR: {}'.format(logs[0])
               ] + ['       {}'.format(log) for log in logs[1:]]
        with self.assertRaises(SystemExit):
            expr()
        self.assertErr(*logs)

    def assertRan(self, *args):
        """Asserts a previous call was made to cli.create_process."""
        self.assertIn(' '.join(args), self.cli.processes.keys())

    def assertInputs(self, args, inputs):
        process = self.get_process(args)
        self.assertEqual(process.inputs, inputs)

    def assertScpTo(self, *args):
        """Asserts a previous call was made to device.scp with args."""
        args = list(args)[:-1] + [self.device.scp_rpath(args[-1])]
        cmd = self._scp_cmd(args)
        self.assertRan(*cmd)

    def assertScpFrom(self, *args):
        """Asserts a previous call was made to device.scp with args."""
        args = [self.device.scp_rpath(arg) for arg in args[:-1]] + [args[-1]]
        cmd = self._scp_cmd(args)
        self.assertRan(*cmd)

    def assertSsh(self, *args):
        """Asserts a previous call was made to device.ssh with cmd."""
        cmd = self._ssh_cmd(list(args))
        self.assertRan(*cmd)


class TestCaseWithFuzzer(TestCaseWithFactory):

    # Unit test "constructor"

    def setUp(self):
        super(TestCaseWithFuzzer, self).setUp()
        self.create_fuzzer('check', 'fake-package1/fake-target1')

    # Unit test context.

    @property
    def ns(self):
        return self.fuzzer.ns

    # Unit test utilities

    def data_abspath(self, relpath):
        """Returns the absolute path for a fuzzer-namespaced data path."""
        return self.fuzzer.ns.abspath(self.fuzzer.ns.data(relpath))