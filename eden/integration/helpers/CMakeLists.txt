# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

if(NOT WIN32)
  # TODO: fake_edenfs currently does not build on Windows, but we should
  # eventually get it working there.
  add_executable(fake_edenfs fake_edenfs.cpp)
  target_link_libraries(
    fake_edenfs
      eden_config
      eden_inodes
      eden_service
  )
endif()

if(NOT WIN32)
  add_executable(takeover_tool TakeoverTool.cpp)
  target_link_libraries(takeover_tool eden_takeover Folly::folly)
endif()

if(WIN32)
  add_executable(read_reparse_buffer ReadReparseBuffer.cpp)
  target_link_libraries(
    read_reparse_buffer
      eden_utils
      edencommon::edencommon_utils
      fmt::fmt
      Folly::folly
  )

  add_executable(check_windows_rename CheckWindowsRename.cpp)
  target_link_libraries(
    read_reparse_buffer
      eden_utils
      edencommon::edencommon_utils
      fmt::fmt
      Folly::folly
  )
endif()
