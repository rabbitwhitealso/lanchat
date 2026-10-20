cmake_minimum_required(VERSION 3.16)
project(lanchat_tray LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 14)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_AUTOMOC ON)

find_package(Qt5 REQUIRED COMPONENTS Widgets)
# 如果是 Windows 平台，引入资源文件
if(WIN32)
    set(APP_ICON_RESOURCE "app.rc")
endif()
# add_executable(lanchat_tray main.cpp)
# 将 app.rc 作为源文件传给 add_executable
add_executable(lanchat_tray main.cpp ${APP_ICON_RESOURCE})
target_link_libraries(lanchat_tray PRIVATE Qt5::Widgets)

if(WIN32)
    set_target_properties(lanchat_tray PROPERTIES WIN32_EXECUTABLE ON)   # 不弹控制台窗口
endif()
if(MSVC)
    target_compile_options(lanchat_tray PRIVATE /utf-8)                  # 源码含中文
endif()
