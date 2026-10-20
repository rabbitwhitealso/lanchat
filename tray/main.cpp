// lanchat_tray —— Qt5 托盘启动器（兼容 Qt 5.12+；Windows / 麒麟 x86 / 麒麟 ARM）
//
// 职责：
//   1. 托盘图标 + 右键菜单（状态点、未读角标、气泡通知）
//   2. 以子进程方式启动/停止/自动重启 Rust 核心程序，路径、文件名、参数可在界面中动态配置
//   3. 与核心通过 stdin/stdout 的「每行一个 JSON」协议通信
//   4. 单实例、开机自启、托盘暂不可用时自动重试（面板晚于本程序启动的情况）
//
// 协议（托盘 -> 核心，写入核心 stdin）：
//   {"cmd":"show"}                      显示主窗口
//   {"cmd":"set_dnd","value":true}      免打扰开关
//   {"cmd":"quit"}                      请核心正常退出
// 协议（核心 -> 托盘，核心写到 stdout，非 JSON 的行当作日志）：
//   {"event":"status","online":3,"port":9000,"dnd":false,"recv_dir":"/home/u/Downloads"}
//   {"event":"unread","count":2}
//   {"event":"message","from":"张三","preview":"你好"}
//   {"event":"transfer","name":"a.zip","percent":42}

#include <QApplication>
#include <QAction>
#include <QDateTime>
#include <QDesktopServices>
#include <QDialog>
#include <QDialogButtonBox>
#include <QDir>
#include <QElapsedTimer>
#include <QFile>
#include <QFileDialog>
#include <QFileInfo>
#include <QFormLayout>
#include <QCheckBox>
#include <QHBoxLayout>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLineEdit>
#include <QLockFile>
#include <QMenu>
#include <QMessageBox>
#include <QPainter>
#include <QPixmap>
#include <QPlainTextEdit>
#include <QProcess>
#include <QPushButton>
#include <QSettings>
#include <QStandardPaths>
#include <QSystemTrayIcon>
#include <QTimer>
#include <QUrl>
#include <QVBoxLayout>

static QString appDir() { return QCoreApplication::applicationDirPath(); }

// ======================= 配置 =======================
struct Config {
    QString corePath = "lanchat";               // 相对路径以托盘程序所在目录为基准；Windows 可省略 .exe
    QStringList args = {"--tray-mode"};         // 传给核心的参数
    QString workDir;                            // 空 = 核心程序所在目录
    bool autoStartCore = true;                  // 托盘启动时自动启动核心
    bool autoRestart = true;                    // 核心异常退出后自动重启（指数退避，最长 30 秒）

    // 便携模式：托盘程序同目录下存在 tray_config.json 就用它，否则用用户配置目录
    static QString filePath() {
        const QString portable = QDir(appDir()).filePath("tray_config.json");
        if (QFileInfo::exists(portable)) return portable;
        const QString dir = QStandardPaths::writableLocation(QStandardPaths::AppConfigLocation);
        QDir().mkpath(dir);
        return QDir(dir).filePath("tray_config.json");
    }

    void load() {
        QFile f(filePath());
        if (!f.open(QIODevice::ReadOnly)) return;
        const QJsonObject o = QJsonDocument::fromJson(f.readAll()).object();
        if (o.contains("corePath")) corePath = o.value("corePath").toString();
        if (o.contains("args")) {
            args.clear();
            const QJsonArray arr = o.value("args").toArray();
            for (const QJsonValue &v : arr) args << v.toString();
        }
        if (o.contains("workDir")) workDir = o.value("workDir").toString();
        if (o.contains("autoStartCore")) autoStartCore = o.value("autoStartCore").toBool();
        if (o.contains("autoRestart")) autoRestart = o.value("autoRestart").toBool();
    }

    bool save() const {
        QJsonObject o;
        o["corePath"] = corePath;
        o["args"] = QJsonArray::fromStringList(args);
        o["workDir"] = workDir;
        o["autoStartCore"] = autoStartCore;
        o["autoRestart"] = autoRestart;
        QFile f(filePath());
        if (!f.open(QIODevice::WriteOnly | QIODevice::Truncate)) return false;
        f.write(QJsonDocument(o).toJson(QJsonDocument::Indented));
        return true;
    }

    // 解析出核心程序的绝对路径
    QString resolvedCore() const {
        QString p = corePath.trimmed();
        if (p.isEmpty()) p = "lanchat";
        QFileInfo fi(p);
        if (fi.isRelative()) fi.setFile(QDir(appDir()).filePath(p));
#ifdef Q_OS_WIN
        if (!fi.exists() && fi.suffix().isEmpty()) fi.setFile(fi.filePath() + ".exe");
#endif
        return QDir::cleanPath(fi.absoluteFilePath());
    }
};

// ======================= 开机自启 =======================
#ifdef Q_OS_WIN
static const char *kRunKey = "HKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Run";
#else
static QString autostartFile() {
    return QStandardPaths::writableLocation(QStandardPaths::ConfigLocation)
           + "/autostart/lanchat-tray.desktop";
}
#endif

static bool autostartEnabled() {
#ifdef Q_OS_WIN
    QSettings s(kRunKey, QSettings::NativeFormat);
    return s.contains("LanChatTray");
#else
    return QFile::exists(autostartFile());
#endif
}

static void setAutostart(bool on) {
#ifdef Q_OS_WIN
    QSettings s(kRunKey, QSettings::NativeFormat);
    if (on) s.setValue("LanChatTray",
                       "\"" + QDir::toNativeSeparators(QCoreApplication::applicationFilePath()) + "\"");
    else s.remove("LanChatTray");
#else
    const QString f = autostartFile();
    if (on) {
        QDir().mkpath(QFileInfo(f).absolutePath());
        QFile file(f);
        if (file.open(QIODevice::WriteOnly | QIODevice::Truncate)) {
            const QString content =
                "[Desktop Entry]\n"
                "Type=Application\n"
                "Name=LanChat Tray\n"
                "Exec=\"" + QCoreApplication::applicationFilePath() + "\"\n"
                "Terminal=false\n"
                "X-GNOME-Autostart-Delay=5\n";
            file.write(content.toUtf8());
        }
    } else {
        QFile::remove(f);
    }
#endif
}

// ======================= 核心进程管理 =======================
class CoreManager : public QObject {
    Q_OBJECT
public:
    explicit CoreManager(Config *cfg, QObject *parent = nullptr) : QObject(parent), cfg_(cfg) {
        connect(&proc_, &QProcess::readyReadStandardOutput, this, &CoreManager::onStdout);
        connect(&proc_, &QProcess::readyReadStandardError, this, &CoreManager::onStderr);
        connect(&proc_, QOverload<int, QProcess::ExitStatus>::of(&QProcess::finished),
                this, &CoreManager::onFinished);
        connect(&proc_, &QProcess::errorOccurred, this, &CoreManager::onError);
        connect(&proc_, &QProcess::started, this, [this] {
            startedAt_.start();
            addLog(tr("核心已启动 (pid %1)").arg(proc_.processId()));
            emit stateChanged(true, tr("运行中"), false);
        });
        restartTimer_.setSingleShot(true);
        connect(&restartTimer_, &QTimer::timeout, this, &CoreManager::start);
    }

    bool running() const { return proc_.state() != QProcess::NotRunning; }
    QStringList logs() const { return logs_; }

public slots:
    void start() {
        if (running()) return;
        stopping_ = false;
        const QString exe = cfg_->resolvedCore();
        const QFileInfo fi(exe);
        if (!fi.exists()) { fail(tr("找不到核心程序: %1").arg(exe)); return; }
#ifndef Q_OS_WIN
        if (!fi.isExecutable()) { fail(tr("核心程序没有执行权限(请 chmod +x): %1").arg(exe)); return; }
#endif
        proc_.setProgram(exe);
        proc_.setArguments(cfg_->args);
        proc_.setWorkingDirectory(cfg_->workDir.isEmpty() ? fi.absolutePath() : cfg_->workDir);
        QProcessEnvironment env = QProcessEnvironment::systemEnvironment();
        env.insert("LANCHAT_TRAY", "1");
        proc_.setProcessEnvironment(env);
        addLog(tr("启动: %1 %2").arg(exe, cfg_->args.join(QStringLiteral(" "))));
        proc_.start();
    }

    void stop() {
        restartTimer_.stop();
        if (!running()) return;
        stopping_ = true;
        send(QJsonObject{{"cmd", "quit"}});
        proc_.closeWriteChannel();                       // stdin 关闭 = 核心应自行退出
        if (!proc_.waitForFinished(3000)) {
            proc_.terminate();
            if (!proc_.waitForFinished(2000)) {
                proc_.kill();
                proc_.waitForFinished(1000);
            }
        }
    }

    void restart() { stop(); start(); }

    void send(const QJsonObject &o) {
        if (proc_.state() != QProcess::Running) return;
        proc_.write(QJsonDocument(o).toJson(QJsonDocument::Compact) + "\n");
    }

signals:
    void stateChanged(bool running, const QString &text, bool isError);
    void eventReceived(const QJsonObject &ev);
    void logLine(const QString &line);

private:
    void fail(const QString &msg) {
        addLog(msg);
        emit stateChanged(false, msg, true);
    }

    void addLog(const QString &s) {
        const QString line = QDateTime::currentDateTime().toString("HH:mm:ss ") + s;
        logs_ << line;
        while (logs_.size() > 500) logs_.removeFirst();
        emit logLine(line);
    }

    void onStdout() {
        buf_ += proc_.readAllStandardOutput();
        int idx;
        while ((idx = buf_.indexOf('\n')) >= 0) {
            const QByteArray line = buf_.left(idx).trimmed();
            buf_.remove(0, idx + 1);
            if (line.isEmpty()) continue;
            QJsonParseError pe;
            const QJsonDocument d = QJsonDocument::fromJson(line, &pe);
            if (pe.error == QJsonParseError::NoError && d.isObject() && d.object().contains("event"))
                emit eventReceived(d.object());
            else
                addLog(QString::fromUtf8(line));
        }
    }

    void onStderr() {
        const QString text = QString::fromUtf8(proc_.readAllStandardError());
        const QStringList lines = text.split('\n');
        for (const QString &l : lines)
            if (!l.trimmed().isEmpty()) addLog("[err] " + l.trimmed());
    }

    void onFinished(int code, QProcess::ExitStatus) {
        buf_.clear();
        if (stopping_) {
            addLog(tr("核心已停止"));
            emit stateChanged(false, tr("已停止"), false);
            return;
        }
        if (startedAt_.isValid() && startedAt_.elapsed() > 30000) backoffMs_ = 1000;
        addLog(tr("核心意外退出 (代码 %1)").arg(code));
        emit stateChanged(false, tr("已退出(代码 %1)").arg(code), true);
        if (cfg_->autoRestart) {
            addLog(tr("%1 毫秒后自动重启").arg(backoffMs_));
            restartTimer_.start(backoffMs_);
            backoffMs_ = qMin(backoffMs_ * 2, 30000);
        }
    }

    void onError(QProcess::ProcessError e) {
        if (e == QProcess::FailedToStart)
            fail(tr("启动失败: %1（常见原因：CPU 架构不匹配、缺少依赖库）").arg(proc_.errorString()));
    }

    Config *cfg_;
    QProcess proc_;
    QTimer restartTimer_;
    QElapsedTimer startedAt_;
    QByteArray buf_;
    QStringList logs_;
    int backoffMs_ = 1000;
    bool stopping_ = false;
};

// ======================= 设置对话框 =======================
class SettingsDialog : public QDialog {
    Q_OBJECT
public:
    explicit SettingsDialog(const Config &c, QWidget *parent = nullptr) : QDialog(parent) {
        setWindowTitle(tr("托盘设置"));
        resize(520, 10);
        pathEdit_ = new QLineEdit(c.corePath);
        auto *browse = new QPushButton(tr("浏览…"));
        auto *row = new QHBoxLayout;
        row->addWidget(pathEdit_, 1);
        row->addWidget(browse);
        argsEdit_ = new QLineEdit(c.args.join(QStringLiteral(" ")));
        workEdit_ = new QLineEdit(c.workDir);
        autoStart_ = new QCheckBox(tr("托盘启动时自动启动核心程序"));
        autoStart_->setChecked(c.autoStartCore);
        autoRestart_ = new QCheckBox(tr("核心异常退出后自动重启"));
        autoRestart_->setChecked(c.autoRestart);
        auto *testBtn = new QPushButton(tr("检查路径"));

        auto *form = new QFormLayout(this);
        form->addRow(tr("核心程序:"), row);
        form->addRow(tr("启动参数:"), argsEdit_);
        form->addRow(tr("工作目录:"), workEdit_);
        form->addRow(autoStart_);
        form->addRow(autoRestart_);
        form->addRow(testBtn);
        auto *box = new QDialogButtonBox(QDialogButtonBox::Ok | QDialogButtonBox::Cancel);
        form->addRow(box);

        connect(box, &QDialogButtonBox::accepted, this, &QDialog::accept);
        connect(box, &QDialogButtonBox::rejected, this, &QDialog::reject);
        connect(browse, &QPushButton::clicked, this, [this] {
            const QString f = QFileDialog::getOpenFileName(this, tr("选择核心程序"), pathEdit_->text());
            if (!f.isEmpty()) pathEdit_->setText(f);
        });
        connect(testBtn, &QPushButton::clicked, this, [this] {
            Config t;
            t.corePath = pathEdit_->text();
            const QFileInfo fi(t.resolvedCore());
            QString msg = tr("可用");
            if (!fi.exists()) msg = tr("文件不存在");
#ifndef Q_OS_WIN
            else if (!fi.isExecutable()) msg = tr("没有执行权限，请执行 chmod +x");
#endif
            QMessageBox::information(this, tr("检查结果"),
                                     tr("解析后的路径:\n%1\n\n结果: %2").arg(fi.absoluteFilePath(), msg));
        });
    }

    Config result(Config base) const {
        base.corePath = pathEdit_->text().trimmed();
        const QString a = argsEdit_->text().simplified();
        base.args = a.isEmpty() ? QStringList() : a.split(' ');
        base.workDir = workEdit_->text().trimmed();
        base.autoStartCore = autoStart_->isChecked();
        base.autoRestart = autoRestart_->isChecked();
        return base;
    }

private:
    QLineEdit *pathEdit_, *argsEdit_, *workEdit_;
    QCheckBox *autoStart_, *autoRestart_;
};

// ======================= 托盘主体 =======================
class TrayApp : public QObject {
    Q_OBJECT
public:
    TrayApp() {
        cfg_.load();
        core_ = new CoreManager(&cfg_, this);
        buildMenu();

        connect(core_, &CoreManager::stateChanged, this,
                [this](bool run, const QString &text, bool err) {
                    lastState_ = text;
                    coreError_ = err;
                    if (!run) { online_ = 0; unread_ = 0; transfer_.clear(); }
                    if (run && pendingShow_) { pendingShow_ = false; core_->send(QJsonObject{{"cmd", "show"}}); }
                    if (err && trayVisible_)
                        tray_.showMessage(tr("核心程序异常"), text, QSystemTrayIcon::Warning, 6000);
                    refresh();
                });
        connect(core_, &CoreManager::eventReceived, this, &TrayApp::onEvent);
        connect(&tray_, &QSystemTrayIcon::activated, this,
                [this](QSystemTrayIcon::ActivationReason r) {
                    if (r == QSystemTrayIcon::Trigger || r == QSystemTrayIcon::DoubleClick) showMain();
                });

        // 托盘暂不可用（面板还没起来）时每 3 秒重试
        connect(&trayRetry_, &QTimer::timeout, this, &TrayApp::tryShowTray);
        trayRetry_.setInterval(3000);
        refresh();
        tryShowTray();
        if (!trayVisible_) trayRetry_.start();

        if (cfg_.autoStartCore) core_->start();
    }

    ~TrayApp() override { core_->stop(); }

public:
    void showMain() {
        if (!core_->running()) { pendingShow_ = true; core_->start(); }
        else core_->send(QJsonObject{{"cmd", "show"}});
    }

private:
    void tryShowTray() {
        if (QSystemTrayIcon::isSystemTrayAvailable()) {
            tray_.show();
            trayVisible_ = true;
            trayRetry_.stop();
            qInfo("托盘已显示");
        } else if (!warned_) {
            warned_ = true;
            qWarning("系统托盘暂不可用，将每 3 秒重试");
        }
    }

    void buildMenu() {
        stateAct_ = menu_.addAction(tr("核心: 未启动"));
        stateAct_->setEnabled(false);
        menu_.addSeparator();
        menu_.addAction(tr("显示主窗口"), this, &TrayApp::showMain);
        dndAct_ = menu_.addAction(tr("免打扰"));
        dndAct_->setCheckable(true);
        connect(dndAct_, &QAction::triggered, this, [this](bool on) {
            core_->send(QJsonObject{{"cmd", "set_dnd"}, {"value", on}});
        });
        menu_.addAction(tr("打开接收文件夹"), this, &TrayApp::openRecvDir);
        menu_.addSeparator();

        QMenu *coreMenu = menu_.addMenu(tr("核心程序"));
        coreMenu->addAction(tr("启动"), core_, &CoreManager::start);
        coreMenu->addAction(tr("停止"), core_, &CoreManager::stop);
        coreMenu->addAction(tr("重启"), core_, &CoreManager::restart);

        menu_.addAction(tr("设置…"), this, &TrayApp::openSettings);
        menu_.addAction(tr("查看日志…"), this, &TrayApp::showLogs);
        autoAct_ = menu_.addAction(tr("开机自启"));
        autoAct_->setCheckable(true);
        autoAct_->setChecked(autostartEnabled());
        connect(autoAct_, &QAction::triggered, this, [](bool on) { setAutostart(on); });
        menu_.addSeparator();
        menu_.addAction(tr("退出"), this, &TrayApp::quitApp);
        tray_.setContextMenu(&menu_);
    }

    void onEvent(const QJsonObject &e) {
        const QString ev = e.value("event").toString();
        if (ev == "status") {
            online_ = e.value("online").toInt();
            recvDir_ = e.value("recv_dir").toString();
            dndAct_->setChecked(e.value("dnd").toBool());
            dnd_ = e.value("dnd").toBool();
        } else if (ev == "unread") {
            unread_ = e.value("count").toInt();
        } else if (ev == "message") {
            if (!dnd_ && trayVisible_)
                tray_.showMessage(e.value("from").toString(), e.value("preview").toString(),
                                  QSystemTrayIcon::Information, 5000);
        } else if (ev == "transfer") {
            const int pct = e.value("percent").toInt();
            transfer_ = pct >= 100 ? QString()
                                   : tr("传输 %1: %2%").arg(e.value("name").toString()).arg(pct);
        }
        refresh();
    }

    void refresh() {
        const bool run = core_->running();
        const QString st = run ? tr("运行中") : (lastState_.isEmpty() ? tr("未运行") : lastState_);
        stateAct_->setText(tr("核心: %1").arg(st));

        QString tip = tr("LanChat\n核心: %1").arg(st);
        if (run) {
            tip += tr("\n在线好友: %1").arg(online_);
            if (unread_ > 0) tip += tr("\n未读消息: %1").arg(unread_);
            if (!transfer_.isEmpty()) tip += "\n" + transfer_;
        }
        tray_.setToolTip(tip);

        const QColor dot = run ? QColor(46, 160, 67)
                               : (coreError_ ? QColor(215, 58, 73) : QColor(140, 140, 140));
        tray_.setIcon(makeIcon(dot, run ? unread_ : 0));
    }

    // 图标：优先使用同目录 tray.png，否则画一个蓝色圆标；右下角状态点，右上角未读角标
    QIcon makeIcon(const QColor &dot, int unread) const {
        QPixmap pm(64, 64);
        pm.fill(Qt::transparent);
        QPainter p(&pm);
        p.setRenderHint(QPainter::Antialiasing);
        const QPixmap custom(QDir(appDir()).filePath("tray.png"));
        if (!custom.isNull()) {
            p.drawPixmap(0, 0, custom.scaled(64, 64, Qt::KeepAspectRatio, Qt::SmoothTransformation));
        } else {
            p.setPen(Qt::NoPen);
            p.setBrush(QColor(52, 120, 246));
            p.drawEllipse(2, 2, 60, 60);
            p.setPen(Qt::white);
            QFont f = p.font();
            f.setBold(true);
            f.setPixelSize(36);
            p.setFont(f);
            p.drawText(QRect(0, 0, 64, 64), Qt::AlignCenter, "L");
        }
        p.setPen(QPen(Qt::white, 3));
        p.setBrush(dot);
        p.drawEllipse(36, 36, 26, 26);
        if (unread > 0) {
            p.setPen(Qt::NoPen);
            p.setBrush(QColor(255, 140, 0));
            p.drawEllipse(26, 0, 38, 38);
            p.setPen(Qt::white);
            QFont f = p.font();
            f.setBold(true);
            f.setPixelSize(unread > 9 ? 20 : 26);
            p.setFont(f);
            p.drawText(QRect(26, 0, 38, 38), Qt::AlignCenter,
                       unread > 99 ? QString("99+") : QString::number(unread));
        }
        p.end();
        return QIcon(pm);
    }

    void openRecvDir() {
        if (recvDir_.isEmpty()) {
            if (trayVisible_)
                tray_.showMessage(tr("提示"), tr("核心尚未上报接收文件夹"), QSystemTrayIcon::Information, 3000);
            return;
        }
        QDesktopServices::openUrl(QUrl::fromLocalFile(recvDir_));
    }

    void openSettings() {
        SettingsDialog dlg(cfg_);
        if (dlg.exec() != QDialog::Accepted) return;
        const Config n = dlg.result(cfg_);
        const bool changed = n.corePath != cfg_.corePath || n.args != cfg_.args || n.workDir != cfg_.workDir;
        cfg_ = n;
        cfg_.save();
        if (changed) core_->restart();
    }

    void showLogs() {
        auto *dlg = new QDialog;
        dlg->setAttribute(Qt::WA_DeleteOnClose);
        dlg->setWindowTitle(tr("核心日志"));
        dlg->resize(720, 460);
        auto *edit = new QPlainTextEdit;
        edit->setReadOnly(true);
        edit->setPlainText(core_->logs().join(QStringLiteral("\n")));
        auto *lay = new QVBoxLayout(dlg);
        lay->addWidget(edit);
        connect(core_, &CoreManager::logLine, edit, &QPlainTextEdit::appendPlainText);
        dlg->show();
    }

    void quitApp() {
        core_->stop();
        tray_.hide();
        qApp->quit();
    }

    Config cfg_;
    CoreManager *core_ = nullptr;
    QMenu menu_;
    QSystemTrayIcon tray_;
    QTimer trayRetry_;
    QAction *stateAct_ = nullptr, *dndAct_ = nullptr, *autoAct_ = nullptr;
    QString lastState_, recvDir_, transfer_;
    int online_ = 0, unread_ = 0;
    bool dnd_ = false, coreError_ = false, trayVisible_ = false, warned_ = false, pendingShow_ = false;
};

int main(int argc, char *argv[]) {
    QApplication app(argc, argv);
    app.setApplicationName("lanchat-tray");
    app.setQuitOnLastWindowClosed(false);   // 关闭设置/日志窗口不能让托盘退出

    // 单实例
    QLockFile lock(QDir::tempPath() + "/lanchat_tray_"
                   + qEnvironmentVariable("USER", qEnvironmentVariable("USERNAME")) + ".lock");
    if (!lock.tryLock(100)) {
        qWarning("托盘程序已在运行");
        return 0;
    }

    TrayApp tray;
    return app.exec();
}

#include "main.moc"
