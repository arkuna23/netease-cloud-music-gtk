//
// toplist.rs
// Copyright (C) 2022 gmg137 <gmg137 AT live.com>
// Distributed under terms of the GPL-3.0-or-later license.
//
use crate::{
    application::Action, gui::songlist_view::SongListView, model::ImageDownloadImpl, path::CACHE,
    utils::*,
};
use adw::{ActionRow, prelude::ActionRowExt, subclass::prelude::BinImpl};
use async_channel::Sender;
use gettextrs::gettext;
use gtk::{CompositeTemplate, glib, prelude::*, subclass::prelude::*, *};
use ncm_api::{SongInfo, TopList};
use once_cell::sync::OnceCell;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

glib::wrapper! {
    pub struct TopListView(ObjectSubclass<imp::TopListView>)
        @extends adw::Bin, Widget, Paned,
        @implements Accessible, Orientable, ConstraintTarget,Buildable;
}

impl Default for TopListView {
    fn default() -> Self {
        Self::new()
    }
}

impl TopListView {
    pub fn new() -> Self {
        glib::Object::new()
    }

    pub fn set_sender(&self, sender: Sender<Action>) {
        self.imp().sender.set(sender).unwrap();
    }

    pub fn init_sidebar(&self, list: Vec<TopList>) {
        let imp = self.imp();
        let sidebar = imp.sidebar.get();
        let sender = imp.sender.get().unwrap();

        let mut select = false;
        for t in &list {
            let action = ActionRow::builder()
                .activatable(true)
                .title(glib::markup_escape_text(&t.name))
                .title_lines(1)
                .subtitle(&t.update)
                .build();
            let mut path = CACHE.clone();
            path.push(format!("{}-toplist.jpg", t.id));
            let image = gtk::Image::from_icon_name("image-missing-symbolic");

            // download cover
            if !path.exists() {
                image.set_from_net(t.cover.to_owned(), path, (140, 140), sender);
            } else {
                image.set_from_file(Some(&path));
            }

            image.set_pixel_size(40);
            action.add_prefix(&image);
            sidebar.append(&action);
            if !select {
                sidebar.select_row(Some(&action));
                select = true;

                // 加载初始选中的榜单封面
                let mut path = CACHE.clone();
                path.push(format!("{}-toplist.jpg", t.id));
                imp.cover_image
                    .set_from_net(t.cover.to_owned(), path, (140, 140), sender);
            }
        }
        imp.data.set(list).unwrap();
        imp.update_toplist_info(0);
    }

    pub fn update_songs_list(&self, sis: &[SongInfo], likes: &[bool]) {
        let imp = self.imp();

        imp.playlist.replace(Clone::clone(&sis).to_vec());
        imp.num_label.get().set_label(&gettext_f(
            "{num} songs",
            &[("num", &sis.len().to_string())],
        ));
        let sender = imp.sender.get().unwrap();
        let songs_list = imp.songs_list.get();
        songs_list.set_sender(sender.clone());
        songs_list.init_new_list(sis, likes);
        songs_list.set_property("no-act-remove", true);

        // 订阅窗口当前播放歌曲变化，使 ▶️ 指示符跟随播放进度。
        if let Some(window) = self
            .root()
            .and_downcast::<crate::window::NeteaseCloudMusicGtk4Window>()
        {
            if !imp.subscribed.get() {
                imp.subscribed.set(true);
                let songs_list = songs_list.downgrade();
                window.connect_local("current-song-changed", false, move |args| {
                    let id = args[1].get::<u64>().unwrap_or(0);
                    if let Some(songs_list) = songs_list.upgrade() {
                        songs_list.update_playing_song(id);
                    }
                    None
                });
            }
            songs_list.update_playing_song(window.current_song_id());
        }
    }
}

mod imp {

    use super::*;

    #[derive(Debug, Default, CompositeTemplate)]
    #[template(resource = "/com/gitee/gmg137/NeteaseCloudMusicGtk4/gtk/toplist.ui")]
    pub struct TopListView {
        #[template_child]
        pub sidebar: TemplateChild<ListBox>,
        #[template_child]
        pub cover_image: TemplateChild<Image>,
        #[template_child]
        pub title_label: TemplateChild<Label>,
        #[template_child]
        pub num_label: TemplateChild<Label>,
        #[template_child]
        pub play_button: TemplateChild<Button>,

        #[template_child]
        pub songs_list: TemplateChild<SongListView>,

        pub playlist: Rc<RefCell<Vec<SongInfo>>>,
        pub data: OnceCell<Vec<TopList>>,
        pub sender: OnceCell<Sender<Action>>,

        pub subscribed: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TopListView {
        const NAME: &'static str = "TopListView";
        type Type = super::TopListView;
        type ParentType = adw::Bin;

        fn class_init(klass: &mut Self::Class) {
            Self::bind_template(klass);
            klass.bind_template_callbacks();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    #[gtk::template_callbacks]
    impl TopListView {
        #[template_callback]
        fn sidebar_cb(&self, row: &ListBoxRow) {
            let index = row.index();
            self.update_toplist_info(index);
        }

        #[template_callback]
        fn play_button_clicked_cb(&self) {
            let sender = self.sender.get().unwrap();
            if !self.playlist.borrow().is_empty() {
                let playlist = &*self.playlist.borrow();
                sender
                    .send_blocking(Action::AddPlayList(
                        playlist.clone(),
                        true,
                        self.songs_list.playback_source(),
                    ))
                    .unwrap();
            } else {
                sender
                    .send_blocking(Action::AddToast(gettext("This is an empty song list！")))
                    .unwrap();
            }
        }

        pub fn update_toplist_info(&self, index: i32) {
            let songs_list = self.songs_list.get();
            songs_list.set_property("no-act-remove", true);
            songs_list.clear_list();

            let data = self.data.get().unwrap();
            if let Some(info) = data.get(index as usize) {
                songs_list.set_source(ncm_api::PlaybackSource {
                    id: info.id.to_string(),
                    kind: "list".into(),
                    name: "list".into(),
                });
                self.sender
                    .get()
                    .unwrap()
                    .send_blocking(Action::GetToplistSongsList(info.id))
                    .unwrap();
                let mut path = CACHE.clone();

                path.push(format!("{}-toplist.jpg", info.id));
                self.cover_image.set_from_file(Some(path));

                let title = self.title_label.get();
                title.set_label(&info.name);
            }
        }
    }

    impl ObjectImpl for TopListView {
        fn constructed(&self) {
            self.parent_constructed();
            setup_ellipsis_tooltip(&self.title_label.get());
        }
    }
    impl WidgetImpl for TopListView {}
    impl BinImpl for TopListView {}
}
