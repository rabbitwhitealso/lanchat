QT += widgets
CONFIG += c++14
TARGET = lanchat_tray
TEMPLATE = app
SOURCES += main.cpp
win32:CONFIG += windows
msvc:QMAKE_CXXFLAGS += /utf-8
