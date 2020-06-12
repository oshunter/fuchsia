#!/usr/bin/env python3

# Copyright 2020 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import os
import subprocess
import sys
import xml.etree.ElementTree as ET


class Env(object):

    def __init__(self, root_dir):
        self.root_dir = root_dir
        self.integration_dir = os.path.normpath(
            os.path.join(root_dir, 'integration'))
        self.stem_cache = os.path.normpath(
            os.path.join(root_dir, '.fx-sync-from-stem.cache'))


def git(args, cwd):
    return subprocess.check_output(['git'] + args, cwd=cwd).decode()


def jiri(args, cwd):
    return subprocess.check_output(['jiri'] + args, cwd=cwd).decode()


def message(msg):
    print('sync-from-stem:', msg)


def reverse_commits(fuchsia_commits_for_integration):
    """
    Convert a map of "integration_revs -> [fuchsia_revs]" to
    "fuchsia_revs -> integration_revs".
    """
    fuchsia_rev_to_integration = {}
    for integ, fuch_revs in fuchsia_commits_for_integration.items():
        for f in fuch_revs:
            fuchsia_rev_to_integration[f] = integ
    return fuchsia_rev_to_integration


def update_stem_history(env):
    """
    Return a map of "fuchsia_rev -> integration_revs".

    This is a relatively expensive operation, and requires performing a git
    fetch of the intergration repo and parsing the "fuchsia/stem" file. We cache
    results in the locations specified by "env" to speed up future calls.
    """
    message('updating in %s' % env.integration_dir)
    git(['fetch', 'origin', '-q'], cwd=env.integration_dir)
    git(['fetch', 'origin', '-q'], cwd=env.root_dir)
    git(['checkout', 'origin/master', '-q'], cwd=env.integration_dir)
    cur_head = git(['rev-parse', 'HEAD'], cwd=env.integration_dir).strip()
    message('integration now at %s' % cur_head)

    data = {}
    data['integration_commits'] = []
    data['head'] = ''
    data['integration_fuchsia_pairs'] = []
    data['fuchsia_commits_for_integration'] = {}

    # Caches:
    # - integration commit -> fuchsia/stem commit (ordered by integration
    #   commit)
    #
    # - Then, a cache of the range of fuchsia commits between those, i.e:
    #
    # integ0 -> fuchsia0
    #        -> fuchsia0-1
    #        -> fuchsia0-2
    # integ1 -> fuchsia1
    # integ2 -> fuchsia2
    #        -> fuchsia2-1
    #        -> fuchsia2-2
    #        -> fuchsia2-3
    # integ3 -> fuchsia3
    #
    # - And, a reversed version of above from fuchsia to its integration
    #   hash. The reversed isn't cached, it's just reversed on load.

    if os.path.exists(env.stem_cache):
        data = json.load(open(env.stem_cache, 'r'))
        if data['head'] == cur_head:
            # Already up to date.
            return reverse_commits(data['fuchsia_commits_for_integration'])

    if not data['head']:
        integration_revs_to_update = cur_head
    else:
        integration_revs_to_update = data['head'] + '..' + cur_head

    message('getting integration commits from %s' % integration_revs_to_update)
    new_integration_commits = git(
        ['log', '--format=%H', integration_revs_to_update, 'fuchsia/stem'],
        cwd=env.integration_dir).split()
    data['integration_commits'] = (
        new_integration_commits + data['integration_commits'])
    data['head'] = cur_head

    # Now we have an up-to-date list of all integration commits, along
    # with new_integration_commits being the ones that have just been
    # added. Read the manifest for each of the new ones, and then add
    # those to the integration_fuchsia_pairs.

    def get_fuchsia_rev_for_integration_commit(ic):
        manifest_root = ET.fromstring(
            git(['show', ic + ':fuchsia/stem'], cwd=env.integration_dir))
        for project in manifest_root.find('projects'):
            if project.get('name') == 'fuchsia':
                return project.get('revision')
        print(
            'Could not find "fuchsia" project in "fuchia/stem" file at '
            'revision %s.' % ic,
            file=sys.stderr)
        sys.exit(1)

    message(
        'caching fuchsia commits for %d integration commits' %
        len(new_integration_commits))
    new_integration_fuchsia_pairs = []
    for i, ic in enumerate(new_integration_commits):
        print('\r%d/%d' % (i, len(new_integration_commits)), end='')
        new_integration_fuchsia_pairs.append(
            (ic, get_fuchsia_rev_for_integration_commit(ic)))
    print('\r\033[K', end='')  # Clear line.
    data['integration_fuchsia_pairs'] = (
        new_integration_fuchsia_pairs + data['integration_fuchsia_pairs'])

    message('caching fuchsia commit ranges')
    # integration_fuchsia_pairs is the first important mapping that's
    # useful, but we don't know what's between each of the fuchsia
    # commits. For each of the new integration commits, log from its
    # fuchsia commit to the preceding fuchsia commit.
    # For example, if we have integration commits i0, i1, i2, i3, ...
    # each with corresponding fuchsia commits f0, f1, f2, f3, ... and
    # the new ones that were just added above were i0 and i1, then we
    # need to log f0..f1, f1..f2, and f2..f3.

    # Special case for the nothing-cached-yet case -- we need to from
    # the last new commit to the newest old commit (per above comment),
    # but on the first run, there's nothing before the oldest new
    # commit. We don't really care about syncing that far back anyway,
    # so just drop that last one.
    last_num = min(
        len(new_integration_fuchsia_pairs),
        len(data['integration_fuchsia_pairs']) - 1)
    for i in range(last_num):
        cur = data['integration_fuchsia_pairs'][i]
        older = data['integration_fuchsia_pairs'][i + 1]
        fuchsia_cur = cur[1]
        fuchsia_older = older[1]
        print('\r%d/%d' % (i, len(new_integration_fuchsia_pairs)), end='')
        fuchsia_revs = git(
            ['log', '--format=%H', fuchsia_older + '..' + fuchsia_cur],
            cwd=env.root_dir).split()
        data['fuchsia_commits_for_integration'][cur[0]] = fuchsia_revs
    print('\r\033[K', end='')  # Clear line.

    json.dump(data, open(env.stem_cache, 'w'))
    return reverse_commits(data['fuchsia_commits_for_integration'])


def set_jiri_ignore(env, to):
    jiri(['project-config', '-ignore=' + to], cwd=env.root_dir)


def save_and_fix_fuchsia_jiri_project_config(env):
    to_return = None
    for line in jiri(['project-config'], cwd=env.root_dir).splitlines()[1:]:
        if line.startswith('ignore:'):
            left, _, right = line.partition(': ')
            to_return = right.strip()
            break
    else:
        print('Could not find current "ignore" value in jiri project-config',
              file=sys.stderr)
        sys.exit(1)
    set_jiri_ignore(env, 'true')
    return to_return


def to_revision(env, fc, ic):
    prev_ignore = save_and_fix_fuchsia_jiri_project_config(env)
    try:
        message('checking out integration %s...' % ic)
        git(
            ['checkout', '-q', '-B',
             'lock_at_%s' % ic, ic],
            cwd=env.integration_dir)
        message('updating dependencies with jiri...')
        jiri(['update', '-local-manifest=true'], cwd=env.integration_dir)
    finally:
        set_jiri_ignore(env, prev_ignore)


def find_first_non_local_commit(env):
    return git(['merge-base', 'HEAD', 'origin/master'],
               cwd=env.root_dir).strip()


def abort_if_is_dirty(git_repo):
    dirty = git(['status', '--porcelain', '--untracked-files=no'], cwd=git_repo)
    if dirty:
        print(
            '%s has uncommitted changes, aborting' % git_repo, file=sys.stderr)
        sys.exit(1)


def main():
    if len(sys.argv) != 2 or not os.path.isdir(sys.argv[1]):
        print(
            '''usage: sync-from-stem.py fuchsia_dir

Sync integration and deps to a state matching fuchsia.git state.

1. Finds the first commit on the current branch that's been integrated
   upstream (i.e. not your local commits).
2. Then, finds the integration repo commit that would include that found
   commit.
3. Syncs other dependencies to match that integration commit.

In this way, you can checkout, update, rebase, etc. your fuchsia.git
branches and then use this script (rather than jiri update) to update
the rest of the tree to match that branch. This allows working as if
fuchsia.git is the "root" of the tree, even though that data is really
stored in the integration repo.
''',
            file=sys.stderr)
        return 1
    env = Env(os.path.abspath(sys.argv[1]))
    abort_if_is_dirty(env.integration_dir)
    f_to_i = update_stem_history(env)

    fuchsia_rev = find_first_non_local_commit(env)
    to_revision(env, fuchsia_rev, f_to_i[fuchsia_rev])
    message(
        'synced integration to %s, which matches fuchsia rev %s' %
        (f_to_i[fuchsia_rev], fuchsia_rev))
    return 0


if __name__ == '__main__':
    sys.exit(main())