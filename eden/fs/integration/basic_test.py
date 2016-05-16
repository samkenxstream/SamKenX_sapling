# Copyright (c) 2016, Facebook, Inc.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree. An additional grant
# of patent rights can be found in the PATENTS file in the same directory.

from __future__ import absolute_import
from __future__ import division
from __future__ import print_function
from __future__ import unicode_literals
import os
import stat
from eden.fs.integration import testcase


class BasicTest(testcase.EdenTestCase):
    '''Exercise some fundamental properties of the filesystem.

    Listing directories, checking stat information, asserting
    that the filesystem is reporting the basic information
    about the sample git repo and that it is correct are all
    things that are appropriate to include in this test case.
    '''
    def test_fileList(self):
        eden = self.init_git_eden()

        entries = sorted(os.listdir(eden.mount_path))
        self.assertEqual(['adir', 'hello', 'slink'], entries)

        adir = os.path.join(eden.mount_path, 'adir')
        st = os.lstat(adir)
        self.assertTrue(stat.S_ISDIR(st.st_mode))
        self.assertEqual(st.st_uid, os.getuid())
        self.assertEqual(st.st_gid, os.getgid())

        hello = os.path.join(eden.mount_path, 'hello')
        st = os.lstat(hello)
        self.assertTrue(stat.S_ISREG(st.st_mode))

        slink = os.path.join(eden.mount_path, 'slink')
        st = os.lstat(slink)
        self.assertTrue(stat.S_ISLNK(st.st_mode))

    def test_symlinks(self):
        eden = self.init_git_eden()

        slink = os.path.join(eden.mount_path, 'slink')
        self.assertEqual(os.readlink(slink), 'hello')

    def test_regular(self):
        eden = self.init_git_eden()

        hello = os.path.join(eden.mount_path, 'hello')
        with open(hello, 'r') as f:
            self.assertEqual('hola\n', f.read())

    def test_dir(self):
        eden = self.init_git_eden()

        entries = sorted(os.listdir(os.path.join(eden.mount_path, 'adir')))
        self.assertEqual(['file'], entries)

        filename = os.path.join(eden.mount_path, 'adir', 'file')
        with open(filename, 'r') as f:
            self.assertEqual('foo!\n', f.read())

    def test_create(self):
        eden = self.init_git_eden()

        filename = os.path.join(eden.mount_path, 'notinrepo')
        with open(filename, 'w') as f:
            f.write('created\n')

        entries = sorted(os.listdir(eden.mount_path))
        self.assertEqual(['adir', 'hello', 'notinrepo', 'slink'], entries)

        with open(filename, 'r') as f:
            self.assertEqual(f.read(), 'created\n')

        st = os.lstat(filename)
        self.assertEqual(st.st_size, 8)
        self.assertTrue(stat.S_ISREG(st.st_mode))

    def test_overwrite(self):
        eden = self.init_git_eden()

        hello = os.path.join(eden.mount_path, 'hello')
        with open(hello, 'w') as f:
            f.write('replaced\n')

        st = os.lstat(hello)
        self.assertEqual(st.st_size, len('replaced\n'))

    def test_materialize(self):
        eden = self.init_git_eden()

        hello = os.path.join(eden.mount_path, 'hello')
        # Opening for write should materialize the file with the same
        # contents that we expect
        with open(hello, 'r+') as f:
            self.assertEqual('hola\n', f.read())

        st = os.lstat(hello)
        self.assertEqual(st.st_size, len('hola\n'))
